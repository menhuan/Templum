//! Explicit, durable enrollment of confirmed cards. A queue uses the host's existing automation
//! look and the same claim and confirmed task delivery as manual delegation. Only an explicit
//! worker `reply` can finish it; a quiet screen, approval or restart never means success.

use std::path::Path;
use std::time::Instant;

use gpui_kit::component::notification::Notification;
use gpui_kit::{App, AppContext as _, Context};
use templum_canvas::BlockId;
use templum_mcp::Reply;
use templum_mcp::tools;

use super::agents_view::Doing;
use super::bus::Outcome;
use super::control::Actor;
use super::delegate::{NOTICE_GAP, TYPING};
use super::{Space, Workspace, show_notification};
use crate::board::{self, Boards, Card, CardQueue, QueueStatus};

const QUEUE_TIMEOUT: u64 = 24 * 60 * 60;

fn asker(card: u64) -> String {
    format!("card-queue-{card}")
}

/// Only called on the explicit reply path. Text copied from a terminal never reaches it.
fn reported(text: &str) -> (QueueStatus, String) {
    let text = text.trim();
    for (prefix, status) in [
        ("[done]", QueueStatus::Done),
        ("[blocked]", QueueStatus::Blocked),
        ("[failed]", QueueStatus::Failed),
    ] {
        if let Some(result) = text.strip_prefix(prefix)
            && !result.trim().is_empty()
        {
            return (status, result.trim().to_string());
        }
    }
    (
        QueueStatus::Blocked,
        format!(
            "Worker replied without an explicit [done], [blocked] or [failed] outcome; review before requeueing.\n\n{text}"
        ),
    )
}

/// Primary task fields are complete. Only accumulated discussion is bounded, so a large
/// comment history can never displace the agreed scope or acceptance checklist.
fn confirmed_context(card: &Card, data: Option<&Path>) -> String {
    let mut context = card.text(data);
    if !card.checklist.is_empty() {
        context.push_str("\n\nAcceptance checklist:");
        for item in &card.checklist {
            context.push_str(&format!(
                "\n[{}] {}: {}",
                if item.done { "x" } else { " " },
                item.id,
                item.text
            ));
        }
    }
    if !card.comments.is_empty() {
        let history = card
            .comments
            .iter()
            .map(|c| format!("{}: {}", c.by, c.text))
            .collect::<Vec<_>>()
            .join("\n\n");
        context.push_str("\n\nRecent card discussion (earlier comments remain on the card):\n");
        context.push_str(tools::tail(&history, 16 * 1024));
    }
    context
}

impl Workspace {
    pub(super) fn card_queue_active(&self) -> bool {
        self.boards
            .boards
            .iter()
            .flat_map(|b| &b.columns)
            .flat_map(|c| &c.cards)
            .any(|c| {
                c.queue
                    .as_ref()
                    .is_some_and(|q| matches!(q.status, QueueStatus::Queued | QueueStatus::Running))
            })
    }

    fn queue_place(&self, card: u64, workspace: &str) -> Result<(u64, String), Reply> {
        let (board, _) = self
            .boards
            .find_card(card)
            .ok_or_else(|| Reply::err(format!("There's no card {card}.")))?;
        let id = self.space_of(workspace, true)?;
        let space = self.space_named(&id).expect("space_of found it");
        if space.stopped || space.is_remote() || space.frozen {
            return Err(Reply::err(
                "Queue pickup needs a running local workspace whose canvas can be saved.",
            ));
        }
        if !space.canvas.blocks().iter().any(|b| {
            space
                .kanban(b.id)
                .is_some_and(|k| !k.remote && k.board == Some(board))
        }) {
            return Err(Reply::err(format!(
                "No kanban in workspace {id} shows card {card}'s board."
            )));
        }
        Ok((board, id))
    }

    /// Save the enrollment/claim before any text can reach an agent. write_newest serializes
    /// this with background snapshots, so an older snapshot cannot undo a dispatched claim.
    fn keep_queue_change(
        &mut self,
        board: u64,
        card: u64,
        change: impl FnOnce(&mut Boards),
    ) -> Result<(), Reply> {
        let dir = self
            .data_dir
            .as_ref()
            .ok_or_else(|| Reply::err("Automatic card pickup needs a persistent data folder."))?;
        if self.boards_frozen {
            return Err(Reply::err(
                "The boards could not be read; automatic pickup is disabled.",
            ));
        }
        let before = self.boards.clone();
        change(&mut self.boards);
        self.generation += 1;
        if let Err(why) = board::save(dir, &self.boards, self.generation) {
            self.boards = before;
            return Err(Reply::err(format!(
                "Couldn't persist card queue: {why}. No new task was dispatched."
            )));
        }
        self.boards_saved = Some(self.boards.clone());
        // Board undo can restore content, never an older dispatch authorization. Update every
        // snapshot containing these cards, including one that can restore a deleted card.
        let queue = self.boards.card(board, card).and_then(|c| c.queue.clone());
        for past in &mut self.boards_undo {
            past.edit_card(board, card, |c| c.queue = queue.clone());
        }
        Ok(())
    }

    pub(super) fn queue_card(
        &mut self,
        actor: &Actor,
        card: u64,
        workspace: &str,
        cx: &mut Context<Self>,
    ) -> Reply {
        let (board, workspace) = match self.queue_place(card, workspace) {
            Ok(place) => place,
            Err(why) => return why,
        };
        if let Some(queue) = &self
            .boards
            .card(board, card)
            .expect("queue_place found it")
            .queue
        {
            if matches!(queue.status, QueueStatus::Queued | QueueStatus::Running)
                && queue.workspace != workspace
            {
                return Reply::err(format!(
                    "Card {card} is enrolled in workspace {}. Withdraw its queued enrollment there first.",
                    queue.workspace
                ));
            }
            match queue.status {
                QueueStatus::Queued | QueueStatus::Running => {
                    return Reply::ok(format!(
                        "Card {card} is already {} in {workspace}; no second task was created.",
                        queue.status.name()
                    ));
                }
                QueueStatus::Done => {
                    return Reply::err(format!(
                        "Card {card} already completed. Create a separate confirmed card for new work."
                    ));
                }
                _ => {}
            }
        }
        // A blocked task may still be alive after a permissions wait or a delivery ambiguity.
        // Explicit opt-in cannot assign a second task while the old one still owns any worker.
        if self.spaces().any(|s| {
            s.terminals().any(|(_, p)| {
                p.handed
                    .as_ref()
                    .is_some_and(|h| h.by == asker(card) || h.card == Some(card))
            })
        }) {
            return Reply::err(format!(
                "Card {card} still has a live native task. Resolve or end that task before requeueing."
            ));
        }
        let (token, roots) = match actor {
            Actor::Agent => (None, None),
            Actor::Api(caller) => {
                let roots = caller
                    .workspaces
                    .as_ref()
                    .map(|_| self.confined_roots(&self.api_scope(cx).visible(caller)));
                (Some(caller.token), roots)
            }
        };
        let queued = CardQueue {
            workspace: workspace.clone(),
            status: QueueStatus::Queued,
            context: confirmed_context(
                self.boards.card(board, card).expect("queue_place found it"),
                self.data_dir.as_deref(),
            ),
            worker: None,
            task: None,
            result: None,
            waiting: None,
            token,
            roots,
        };
        if let Some(why) = self.queue_authority_problem(&queued) {
            return Reply::err(why);
        }
        if let Err(why) = self.keep_queue_change(board, card, |b| {
            b.edit_card(board, card, |c| c.queue = Some(queued));
        }) {
            return why;
        }
        self.look_at_automations(cx);
        cx.notify();
        Reply::ok(format!(
            "Queued card {card} in workspace {workspace}. The host will pick it up once an eligible connected agent is free."
        ))
    }

    pub(super) fn cancel_queued_card(
        &mut self,
        card: u64,
        workspace: &str,
        cx: &mut Context<Self>,
    ) -> Reply {
        let Some((board, _)) = self.boards.find_card(card) else {
            return Reply::err(format!("There's no card {card}."));
        };
        let workspace = match self.space_of(workspace, true) {
            Ok(id) => id,
            Err(why) => return why,
        };
        let Some(queue) = self.boards.card(board, card).and_then(|c| c.queue.as_ref()) else {
            return Reply::ok(format!("Card {card} is not queued."));
        };
        if queue.workspace != workspace {
            return Reply::err(format!(
                "Card {card} belongs to queue workspace {}.",
                queue.workspace
            ));
        }
        if queue.status == QueueStatus::Running {
            return Reply::err(format!(
                "Card {card} is already running. Inspect its worker and live task before changing its scope."
            ));
        }
        if queue.status != QueueStatus::Queued {
            return Reply::ok(format!(
                "Card {card} is {}; there is no pending pickup to withdraw.",
                queue.status.name()
            ));
        }
        if let Err(why) = self.keep_queue_change(board, card, |b| {
            b.edit_card(board, card, |c| {
                let q = c.queue.as_mut().expect("enrolled");
                q.status = QueueStatus::Cancelled;
                q.result = Some("Pickup withdrawn before dispatch; card retained.".into());
            });
        }) {
            return why;
        }
        cx.notify();
        Reply::ok(format!(
            "Withdrew card {card} from automatic pickup; its card was retained."
        ))
    }

    pub(super) fn manual_card_task_problem(&self, card: u64) -> Option<Reply> {
        let Some((board, _)) = self.boards.find_card(card) else {
            return Some(Reply::err(format!("There's no card {card}.")));
        };
        if let Some(queue) = self.boards.card(board, card).and_then(|c| c.queue.as_ref())
            && queue.status != QueueStatus::Cancelled
        {
            return Some(Reply::err(format!(
                "Card {card} has host pickup state {}. Use queue_card for explicit recovery, or cancel its pending pickup before manual delegation.",
                queue.status.name()
            )));
        }
        self.spaces().any(|s| s.terminals().any(|(_, p)| p.handed.as_ref().is_some_and(|h| h.card == Some(card))))
            .then(|| Reply::err(format!("Card {card} is already bound to a live native task. End that task before handing the card out again.")))
    }

    pub(super) fn queue_workspace_deleted(
        &mut self,
        workspace: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), Reply> {
        let cards: Vec<_> = self
            .boards
            .boards
            .iter()
            .flat_map(|b| {
                b.columns.iter().flat_map(move |col| {
                    col.cards.iter().filter_map(move |c| {
                        c.queue
                            .as_ref()
                            .filter(|q| {
                                q.workspace == workspace
                                    && matches!(
                                        q.status,
                                        QueueStatus::Queued | QueueStatus::Running
                                    )
                            })
                            .map(|_| (b.id, c.id))
                    })
                })
            })
            .collect();
        for (board, card) in cards {
            self.keep_queue_end(board, card, QueueStatus::Blocked, "The enrolled workspace was deleted. Its previous pickup authorization is withdrawn; review the card before explicitly queueing it in another workspace.", cx)?;
        }
        Ok(())
    }

    fn queue_authority_problem(&self, queue: &CardQueue) -> Option<String> {
        if queue.context.trim().is_empty() {
            return Some("The confirmed task context is missing. Review the card and explicitly requeue it before pickup.".into());
        }
        let id = queue.token?;
        let now = crate::notes::now();
        let token = self
            .api
            .store
            .as_ref()
            .and_then(|s| s.tokens.iter().find(|t| t.id == id && t.live(now)));
        match token {
            None => Some("Pickup authorization expired or was revoked; explicitly requeue with current authorization.".into()),
            Some(t) if t.scope < templum_api::Scope::Control || t.workspaces.as_ref().is_some_and(|spaces| !spaces.contains(&queue.workspace)) => Some("Pickup authorization no longer reaches this workspace.".into()),
            Some(_) => None,
        }
    }

    fn queue_worker(
        &self,
        space: &Space,
        board: u64,
        card: u64,
        queue: &CardQueue,
        cx: &App,
    ) -> (Option<(BlockId, String, String)>, Option<String>) {
        let Some(card) = self.boards.card(board, card) else {
            return (None, Some("The enrolled card was removed.".into()));
        };
        let now = Instant::now();
        let mut target_exists = false;
        let mut ready_exists = false;
        let mut not_ready = None;
        let worker = space.terminals().find_map(|(id, pane)| {
            let uid = space.uids.get(&id)?;
            let caller = (uid.clone(), Some(pane.agent()?.to_string()));
            if self.may_take(card, &caller, cx).is_err() {
                return None;
            }
            let wired = space.canvas.blocks().iter().any(|b| {
                space.wired(id, b.id)
                    && space
                        .kanban(b.id)
                        .is_some_and(|k| !k.remote && k.board == Some(board))
            });
            if !wired {
                return None;
            }
            let current_roots = queue
                .token
                .and_then(|token| {
                    self.api
                        .store
                        .as_ref()?
                        .tokens
                        .iter()
                        .find(|t| t.id == token)
                })
                .and_then(|token| token.workspaces.as_ref());
            if queue.roots.is_some() || current_roots.is_some() {
                let path = self.about(&space.blocks[&id], cx).2?;
                let real = dunce::canonicalize(path).ok()?;
                let enrolled = queue
                    .roots
                    .as_ref()
                    .is_none_or(|roots| roots.iter().any(|root| real.starts_with(root)));
                let current = current_roots.is_none_or(|spaces| {
                    spaces
                        .iter()
                        .filter_map(|id| {
                            dunce::canonicalize(self.space_named(id)?.folder.as_ref()?).ok()
                        })
                        .any(|root| real.starts_with(root))
                });
                if !enrolled || !current {
                    return None;
                }
            }
            target_exists = true;
            if !self.has_tools(pane.term.id()) && !self.replies_from_shell(pane) {
                not_ready = Some(format!("Connected worker {uid} cannot explicitly report a result: {}. Restore its Templum tools or connect a worker that can reply.", self.no_tools_why(pane)));
                return None;
            }
            let doing = self.doing(pane);
            if doing == Doing::Starting || pane.agent_starting() {
                not_ready = Some(format!("Connected worker {uid} is still starting; pickup waits for native agent readiness."));
                return None;
            }
            if matches!(doing, Doing::NeedsYou(_) | Doing::Working(_) | Doing::AwaitsSubagents(_)) || pane.handed.is_some() {
                ready_exists = true;
                return None;
            }
            if let Err(why) = pane.takes_handing(uid) {
                not_ready = Some(format!("Connected worker {uid} is not ready to receive a task: {}", why.text));
                return None;
            }
            ready_exists = true;
            let recent = |at: Option<Instant>, within| at.is_some_and(|at| at.elapsed() < within);
            let reserved = self
                .boards
                .boards
                .iter()
                .flat_map(|b| &b.columns)
                .flat_map(|c| &c.cards)
                .any(|c| {
                    c.queue.as_ref().is_some_and(|q| {
                        q.status == QueueStatus::Running && q.worker.as_deref() == Some(uid)
                    })
                });
            if reserved
                || doing != Doing::Idle
                || pane.busy(now)
                || pane.has_draft()
                || recent(pane.keyed.get(), TYPING)
                || recent(pane.sent, NOTICE_GAP)
            {
                return None;
            }
            Some((id, uid.clone(), pane.term.id().to_string()))
        });
        let waiting = if worker.is_some() || ready_exists {
            None
        } else if !target_exists {
            Some("No existing agent in this workspace is wired to this board and matches the card's assignee and authorized folders. Connect or restore an eligible worker; pickup remains queued.".into())
        } else {
            not_ready
        };
        (worker, waiting)
    }

    pub(super) fn look_card_queue(&mut self, cx: &mut Context<Self>) {
        let cards: Vec<_> = self
            .boards
            .boards
            .iter()
            .flat_map(|b| {
                b.columns.iter().flat_map(move |col| {
                    col.cards
                        .iter()
                        .filter_map(move |c| c.queue.clone().map(|q| (b.id, c.id, q)))
                })
            })
            .filter(|(_, _, q)| matches!(q.status, QueueStatus::Queued | QueueStatus::Running))
            .collect();
        // Resolve old workers first. Queued entries never steal an ambiguous worker.
        for (board, card, queue) in cards
            .iter()
            .filter(|(_, _, q)| q.status == QueueStatus::Running)
        {
            let live = queue.worker.as_deref().and_then(|uid| {
                let (space, id) = self.find_block(uid)?;
                if space != queue.workspace {
                    return None;
                }
                let pane = self.space_named(&space)?.terminal(id)?;
                let handed = pane
                    .handed
                    .as_ref()
                    .filter(|h| Some(h.id) == queue.task && h.by == asker(*card))?;
                Some((pane, handed))
            });
            let Some((pane, handed)) = live else {
                self.queue_end(*board, *card, QueueStatus::Blocked, "The native task ended or was lost without an explicit result (including restart). Inspect the card and worker before explicitly requeueing.", cx);
                continue;
            };
            let waiting = match self.doing(pane) {
                Doing::NeedsYou(why) => Some(format!("Worker needs you: {why}")),
                Doing::Idle if handed.quiet => Some("Worker is quiet without an explicit reply; inspect its terminal for permission, a blocker, or its result. The live task is retained.".into()),
                _ if Instant::now() >= handed.until => Some("Native task deadline passed without an explicit result. Inspect its worker; the enrollment will not be dispatched again.".into()),
                _ => None,
            };
            self.queue_wait(*board, *card, queue, waiting, cx);
        }
        for (board, card, queue) in cards
            .into_iter()
            .filter(|(_, _, q)| q.status == QueueStatus::Queued)
        {
            if let Some(why) = self.queue_authority_problem(&queue) {
                self.queue_end(board, card, QueueStatus::Blocked, &why, cx);
                continue;
            }
            let Some(space) = self
                .space_named(&queue.workspace)
                .filter(|s| !s.stopped && !s.is_remote() && !s.frozen)
            else {
                self.queue_wait(board, card, &queue, Some("The enrolled workspace is missing, stopped, remote or cannot be saved. Pickup is paused there.".into()), cx);
                continue;
            };
            let (worker, waiting) = self.queue_worker(space, board, card, &queue, cx);
            if !self.queue_wait(board, card, &queue, waiting, cx) {
                continue;
            }
            let Some((id, uid, terminal)) = worker else {
                continue;
            };
            let original = self.boards.clone();
            let claimed = self.claim_queued_card(&terminal, card, cx);
            if claimed.is_error {
                self.queue_end(board, card, QueueStatus::Blocked, &claimed.text, cx);
                continue;
            }
            let task_id = self.next_task + 1;
            if let Err(why) = self.keep_queue_change(board, card, |b| {
                b.edit_card(board, card, |c| {
                    let q = c.queue.as_mut().expect("enrolled");
                    q.status = QueueStatus::Running;
                    q.worker = Some(uid.clone());
                    q.task = Some(task_id);
                    q.waiting = None;
                });
                move_to(b, board, card, &["Doing", "In progress"]);
            }) {
                self.boards = original;
                // A failed durable claim must never lead to typing; pause it in memory too.
                self.boards.edit_card(board, card, |c| {
                    let q = c.queue.as_mut().expect("enrolled");
                    q.status = QueueStatus::Failed;
                    q.result = Some(why.text.clone());
                });
                self.card_queue_notice(card, &queue, &why.text, cx);
                continue;
            }
            let context = &queue.context;
            let task = format!(
                "Complete and verify the confirmed work on kanban card {card} in workspace {}, within its agreed scope and acceptance criteria. Report once with the existing reply tool: begin text with [done] and concrete results plus verification evidence, [blocked] and the exact blocker plus answer needed, or [failed] and what happened. The host already claimed the card and writes the result, comment and column; no claim/move/comment bookkeeping tools are needed. A live permission wait retains this task; ask the person through the existing native mechanism. Do not pick up another card yourself.\n\n{context}",
                queue.workspace
            );
            let task = if self.has_tools(&terminal) {
                task
            } else {
                format!(
                    "{task}\n\nYour native MCP reply tool is unavailable. Report using the supported `templum reply -` command in your shell, supplying your whole answer on stdin with the same [done], [blocked] or [failed] prefix."
                )
            };
            let outcome = self.hand_task(
                &asker(card),
                "Templum card queue",
                (&queue.workspace, id),
                &uid,
                &task,
                false,
                QUEUE_TIMEOUT,
                None,
                cx,
            );
            match outcome {
                Outcome::Now(reply) if reply.is_error => {
                    self.queue_end(board, card, QueueStatus::Failed, &reply.text, cx)
                }
                Outcome::Later(answer) => {
                    cx.spawn(async move |ws, cx| {
                        let reply = answer.await;
                        if reply.is_error {
                            ws.update(cx, |ws, cx| {
                                if ws
                                    .boards
                                    .card(board, card)
                                    .and_then(|c| c.queue.as_ref())
                                    .is_some_and(|q| {
                                        q.status == QueueStatus::Running && q.task == Some(task_id)
                                    })
                                {
                                    ws.queue_end(
                                        board,
                                        card,
                                        QueueStatus::Blocked,
                                        &reply.text,
                                        cx,
                                    );
                                }
                            })
                            .ok();
                        }
                    })
                    .detach();
                }
                Outcome::Wait(..) => self.queue_end(
                    board,
                    card,
                    QueueStatus::Failed,
                    "Task handoff unexpectedly waited without a delivery receipt.",
                    cx,
                ),
                Outcome::Now(_) => {}
            }
            cx.notify();
        }
    }

    fn card_queue_notice(
        &mut self,
        card: u64,
        queue: &CardQueue,
        text: &str,
        cx: &mut Context<Self>,
    ) {
        let text = format!("Card {card}: {text}");
        self.api_notice(templum_api::model::Notice {
            text: text.clone(),
            workspace: Some(queue.workspace.clone()),
            block: queue.worker.clone(),
            automation: None,
        });
        let window = self.window_handle;
        let space = queue.workspace.clone();
        cx.spawn(async move |ws, cx| {
            cx.update_window(window, |_, window, cx| {
                ws.update(cx, |ws, cx| {
                    let weak = cx.entity().downgrade();
                    let notification = Notification::info(text)
                        .delivery(ws.delivery(window))
                        .on_click(move |_, window, cx| {
                            weak.update(cx, |ws, cx| {
                                let Some((board, _)) = ws.boards.find_card(card) else {
                                    return;
                                };
                                let target = ws.space_named(&space).and_then(|s| {
                                    s.canvas
                                        .blocks()
                                        .iter()
                                        .find(|b| {
                                            s.kanban(b.id).is_some_and(|k| k.board == Some(board))
                                        })
                                        .map(|b| b.id)
                                });
                                if let Some(block) = target {
                                    ws.reveal(&space, block, window, cx);
                                    crate::kanban::open_card(ws, board, card, window, cx);
                                }
                            })
                            .ok();
                        });
                    show_notification(window, cx, notification);
                })
                .ok();
            })
            .ok();
        })
        .detach();
    }

    fn queue_wait(
        &mut self,
        board: u64,
        card: u64,
        queue: &CardQueue,
        waiting: Option<String>,
        cx: &mut Context<Self>,
    ) -> bool {
        if waiting == queue.waiting {
            return true;
        }
        let text = waiting.clone();
        match self.keep_queue_change(board, card, |b| {
            b.edit_card(board, card, |c| {
                c.queue.as_mut().expect("enrolled").waiting = waiting
            });
        }) {
            Ok(()) => {
                if let Some(text) = text {
                    self.card_queue_notice(card, queue, &text, cx);
                }
                cx.notify();
                true
            }
            Err(why) => {
                self.queue_end(board, card, QueueStatus::Failed, &why.text, cx);
                false
            }
        }
    }

    fn keep_queue_end(
        &mut self,
        board: u64,
        card: u64,
        status: QueueStatus,
        result: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), Reply> {
        let Some(queue) = self.boards.card(board, card).and_then(|c| c.queue.clone()) else {
            return Ok(());
        };
        self.keep_queue_change(board, card, |b| {
            b.edit_card(board, card, |c| {
                let q = c.queue.as_mut().expect("enrolled");
                q.status = status;
                q.result = Some(result.to_string());
                q.waiting = None;
                if status == QueueStatus::Done {
                    q.context.clear();
                }
            });
            b.add_comment(
                board,
                card,
                "you",
                &format!("Host queue {}: {result}", status.name()),
                crate::notes::now(),
            );
            if status == QueueStatus::Done {
                move_to(b, board, card, &["Review", "Done"]);
            }
        })?;
        self.card_queue_notice(card, &queue, &format!("{} — {result}", status.name()), cx);
        cx.notify();
        Ok(())
    }

    fn queue_end(
        &mut self,
        board: u64,
        card: u64,
        status: QueueStatus,
        result: &str,
        cx: &mut Context<Self>,
    ) {
        if let Err(why) = self.keep_queue_end(board, card, status, result, cx) {
            if let Some(queue) = self.boards.card(board, card).and_then(|c| c.queue.clone()) {
                self.boards.edit_card(board, card, |c| {
                    let q = c.queue.as_mut().expect("enrolled");
                    q.status = QueueStatus::Failed;
                    q.result = Some(why.text.clone());
                });
                self.card_queue_notice(card, &queue, &why.text, cx);
            }
        }
    }

    pub(super) fn queue_reply(
        &mut self,
        terminal: &str,
        text: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), Reply> {
        let Some(handed) = self.pane(terminal).and_then(|p| p.handed.as_ref()) else {
            return Ok(());
        };
        let Some(card) = handed
            .by
            .strip_prefix("card-queue-")
            .and_then(|id| id.parse::<u64>().ok())
        else {
            return Ok(());
        };
        let Some((board, _)) = self.boards.find_card(card) else {
            return Err(Reply::err(
                "The queued card was removed; tell the person before answering this task.",
            ));
        };
        let uid = self.uid_of(terminal);
        let Some(queue) = self.boards.card(board, card).and_then(|c| c.queue.as_ref()) else {
            return Err(Reply::err("This task's card is no longer enrolled."));
        };
        if queue.task != Some(handed.id) || queue.worker.as_deref() != Some(uid.as_str()) {
            return Err(Reply::err(
                "This reply is not from the card's current worker and task.",
            ));
        }
        let (status, result) = reported(text);
        self.keep_queue_end(board, card, status, &result, cx)
    }
}

fn move_to(boards: &mut Boards, board: u64, card: u64, names: &[&str]) {
    let target = names.iter().find_map(|name| {
        boards
            .board(board)?
            .columns
            .iter()
            .find(|c| c.title.trim().eq_ignore_ascii_case(name))
            .map(|c| c.id)
    });
    if let Some(column) = target {
        boards.move_card_before(board, card, column, None);
    }
}
