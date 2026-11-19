//! Host pickup through the real API, native claim/task and explicit reply paths.

use serde_json::json;
use templum_api::{Scope, Token};
use templum_mcp::Reply;

use super::api::{said, state, tool, with_token};
use super::bus::{call, me};
use super::delegate::agents_running_in;
use super::*;
use crate::board::{Assignee, CardQueue, QueueStatus};
use crate::session::Loaded;

fn setup(
    cx: &mut TestAppContext,
) -> (
    tempfile::TempDir,
    AnyWindowHandle,
    Entity<Workspace>,
    BlockId,
    u64,
    u64,
    Token,
) {
    let data = tempfile::tempdir().unwrap();
    let (handle, ws, _, worker) = agents_running_in(cx, None, Some(data.path().to_path_buf()));
    let (kanban, board, card) = board_with_a_card(handle, &ws, cx);
    ws.update(cx, |ws, cx| {
        assert!(ws.connect(worker, kanban, cx));
        ws.space.terminal_mut(worker).unwrap().state = Some(super::super::AgentState::Idle);
        ws.boards.edit_card(board, card, |c| {
            c.description = "Implement the agreed change; verify its observable result.".into()
        });
    });
    let token = with_token(&ws, cx, Scope::Control, None);
    (data, handle, ws, worker, board, card, token)
}

fn queue(
    handle: AnyWindowHandle,
    ws: &Entity<Workspace>,
    cx: &mut TestAppContext,
    token: &Token,
    card: u64,
) {
    let workspace = ws.read_with(cx, |ws, _| ws.space.id.clone());
    let answer = tool(
        handle,
        ws,
        cx,
        token,
        "queue_card",
        json!({"card": card, "workspace": workspace}),
    );
    assert!(answer["error"].is_null() && !said(&answer).0, "{answer}");
}

fn pickup(ws: &Entity<Workspace>, cx: &mut TestAppContext) {
    ws.update(cx, |ws, cx| ws.look_card_queue(cx));
}

fn status(ws: &Entity<Workspace>, cx: &mut TestAppContext, board: u64, card: u64) -> CardQueue {
    ws.read_with(cx, |ws, _| {
        ws.boards.card(board, card).unwrap().queue.clone().unwrap()
    })
}

#[gpui_kit::test]
fn queue_is_explicit_durable_and_idempotent_and_uses_native_claim(cx: &mut TestAppContext) {
    let (data, handle, ws, worker, board, card, token) = setup(cx);
    let manual = ws.update(cx, |ws, _| {
        let column = ws.boards.board(board).unwrap().columns[0].id;
        ws.boards.add_card(board, column).unwrap()
    });
    pickup(&ws, cx);
    assert!(ws.read_with(cx, |ws, _| {
        ws.space.terminal(worker).unwrap().handed.is_none()
    }));
    queue(handle, &ws, cx, &token, card);
    assert_eq!(status(&ws, cx, board, card).status, QueueStatus::Queued);
    let Loaded::Saved(saved) = crate::board::load(data.path()) else {
        panic!("not persisted before returning");
    };
    assert_eq!(
        saved
            .card(board, card)
            .unwrap()
            .queue
            .as_ref()
            .unwrap()
            .status,
        QueueStatus::Queued
    );
    queue(handle, &ws, cx, &token, card);
    pickup(&ws, cx);
    let running = status(&ws, cx, board, card);
    assert_eq!(running.status, QueueStatus::Running);
    let (terminal, uid) = me(&ws, cx, worker);
    assert_eq!(running.worker, Some(uid.clone()));
    ws.read_with(cx, |ws, _| {
        assert_eq!(
            ws.boards.card(board, card).unwrap().assignee,
            Some(Assignee::Block(uid))
        );
        assert!(ws.boards.card(board, manual).unwrap().queue.is_none());
        let handed = ws.pane(&terminal).unwrap().handed.as_ref().unwrap();
        assert_eq!(Some(handed.id), running.task);
        assert_eq!(handed.by, format!("card-queue-{card}"));
    });
    queue(handle, &ws, cx, &token, card);
    pickup(&ws, cx);
    assert_eq!(
        status(&ws, cx, board, card),
        running,
        "no new task on another tick/call"
    );
    let view = state(handle, &ws, cx, &token);
    assert_eq!(
        view.cards
            .iter()
            .find(|c| c.id == card)
            .unwrap()
            .queue
            .as_ref()
            .unwrap()
            .status,
        "running"
    );
}

#[gpui_kit::test]
fn busy_or_wrong_agent_stays_queued_then_only_one_card_owns_the_worker(cx: &mut TestAppContext) {
    let (_data, handle, ws, worker, board, card, token) = setup(cx);
    ws.update(cx, |ws, _| {
        ws.boards.edit_card(board, card, |c| {
            c.assignee = Some(Assignee::Agent("claude".into()))
        });
    });
    queue(handle, &ws, cx, &token, card);
    pickup(&ws, cx);
    assert_eq!(status(&ws, cx, board, card).status, QueueStatus::Queued);
    assert!(
        status(&ws, cx, board, card).waiting.is_some(),
        "a missing eligible worker is visible"
    );
    let second = ws.update(cx, |ws, _| {
        ws.boards.edit_card(board, card, |c| {
            c.assignee = Some(Assignee::Agent("codex".into()))
        });
        ws.space.terminal_mut(worker).unwrap().state = Some(super::super::AgentState::Working);
        let column = ws.boards.board(board).unwrap().columns[0].id;
        ws.boards.add_card(board, column).unwrap()
    });
    queue(handle, &ws, cx, &token, second);
    pickup(&ws, cx);
    assert_eq!(status(&ws, cx, board, card).status, QueueStatus::Queued);
    assert!(
        status(&ws, cx, board, card).waiting.is_none(),
        "busy is an ordinary wait"
    );
    ws.update(cx, |ws, _| {
        ws.space.terminal_mut(worker).unwrap().state = Some(super::super::AgentState::Idle)
    });
    pickup(&ws, cx);
    let states = [
        status(&ws, cx, board, card).status,
        status(&ws, cx, board, second).status,
    ];
    assert_eq!(
        states
            .iter()
            .filter(|s| **s == QueueStatus::Running)
            .count(),
        1
    );
    assert_eq!(
        states.iter().filter(|s| **s == QueueStatus::Queued).count(),
        1
    );
}

#[gpui_kit::test]
fn cancel_retains_card_and_explicit_blocked_can_resume_but_done_never_repeats(
    cx: &mut TestAppContext,
) {
    let (_data, handle, ws, worker, board, card, token) = setup(cx);
    let workspace = ws.read_with(cx, |ws, _| ws.space.id.clone());
    queue(handle, &ws, cx, &token, card);
    let cancelled = tool(
        handle,
        &ws,
        cx,
        &token,
        "cancel_queued_card",
        json!({"card": card, "workspace": workspace}),
    );
    assert!(!said(&cancelled).0, "{cancelled}");
    pickup(&ws, cx);
    assert_eq!(status(&ws, cx, board, card).status, QueueStatus::Cancelled);
    queue(handle, &ws, cx, &token, card);
    pickup(&ws, cx);
    let denied = tool(
        handle,
        &ws,
        cx,
        &token,
        "cancel_queued_card",
        json!({"card": card, "workspace": workspace}),
    );
    assert!(said(&denied).0);
    let (terminal, _) = me(&ws, cx, worker);
    let (failed, reply) = call(
        handle,
        &ws,
        cx,
        &terminal,
        "reply",
        json!({"text": "[blocked] Need the approved API endpoint."}),
    );
    assert!(!failed, "{reply}");
    assert_eq!(status(&ws, cx, board, card).status, QueueStatus::Blocked);
    pickup(&ws, cx);
    assert_eq!(status(&ws, cx, board, card).status, QueueStatus::Blocked);
    ws.update(cx, |ws, _| {
        ws.boards.edit_card(board, card, |c| {
            c.description.push_str("\nApproved endpoint: /v2/search.")
        });
        ws.space.terminal_mut(worker).unwrap().sent = None;
    });
    queue(handle, &ws, cx, &token, card);
    pickup(&ws, cx);
    assert_eq!(status(&ws, cx, board, card).status, QueueStatus::Running);
    let (failed, reply) = call(
        handle,
        &ws,
        cx,
        &terminal,
        "reply",
        json!({"text": "[done] Implemented /v2/search and verified the response."}),
    );
    assert!(!failed, "{reply}");
    let done = status(&ws, cx, board, card);
    assert_eq!(done.status, QueueStatus::Done);
    assert!(done.result.unwrap().contains("verified"));
    ws.update(cx, |ws, cx| {
        assert!(ws.undo_boards(cx));
    });
    pickup(&ws, cx);
    assert_eq!(
        status(&ws, cx, board, card).status,
        QueueStatus::Done,
        "undo cannot revive old enrollment"
    );
    let refused = tool(
        handle,
        &ws,
        cx,
        &token,
        "queue_card",
        json!({"card": card, "workspace": workspace}),
    );
    assert!(said(&refused).0, "{refused}");
}

#[gpui_kit::test]
fn implicit_idle_and_restart_never_turn_screen_text_into_done(cx: &mut TestAppContext) {
    let (data, handle, ws, worker, board, card, token) = setup(cx);
    queue(handle, &ws, cx, &token, card);
    pickup(&ws, cx);
    let (terminal, _) = me(&ws, cx, worker);
    ws.update(cx, |ws, cx| {
        ws.end_turn(
            &terminal,
            Reply::ok("[done] copied from terminal screen"),
            cx,
        )
    });
    pickup(&ws, cx);
    assert_eq!(status(&ws, cx, board, card).status, QueueStatus::Blocked);
    // A crash after a durable claim but before the delivery receipt cannot send it twice.
    ws.update(cx, |ws, _| {
        ws.space.terminal_mut(worker).unwrap().sent = None
    });
    queue(handle, &ws, cx, &token, card);
    pickup(&ws, cx);
    assert_eq!(status(&ws, cx, board, card).status, QueueStatus::Running);
    let (_, restored) = open_in(cx, None, Some(data.path().to_path_buf()));
    pickup(&restored, cx);
    assert_eq!(
        status(&restored, cx, board, card).status,
        QueueStatus::Blocked
    );
    pickup(&restored, cx);
    assert_eq!(
        status(&restored, cx, board, card).status,
        QueueStatus::Blocked
    );
}

#[gpui_kit::test]
fn permission_and_quiet_waits_keep_task_and_notify_once_in_background(cx: &mut TestAppContext) {
    let (_data, handle, ws, worker, board, card, token) = setup(cx);
    cx.update(|cx| cx.set_app_identity("test.Templum", "Templum"));
    super::terminals::set_active(handle, false, cx);
    queue(handle, &ws, cx, &token, card);
    pickup(&ws, cx);
    ws.update(cx, |ws, _| {
        ws.space.terminal_mut(worker).unwrap().state = Some(super::super::AgentState::NeedsYou(
            Some("Approve tool?".into()),
        ))
    });
    pickup(&ws, cx);
    pickup(&ws, cx);
    cx.run_until_parked();
    assert_eq!(status(&ws, cx, board, card).status, QueueStatus::Running);
    assert!(
        status(&ws, cx, board, card)
            .waiting
            .unwrap()
            .contains("Approve tool")
    );
    assert_eq!(
        cx.shown_system_notifications().len(),
        1,
        "one system notification per live wait"
    );
    ws.update(cx, |ws, _| {
        let pane = ws.space.terminal_mut(worker).unwrap();
        pane.state = Some(super::super::AgentState::Idle);
        pane.handed.as_mut().unwrap().quiet = true;
    });
    pickup(&ws, cx);
    pickup(&ws, cx);
    cx.run_until_parked();
    assert_eq!(
        cx.shown_system_notifications().len(),
        2,
        "quiet without hooks is also told, once"
    );
    assert!(
        status(&ws, cx, board, card)
            .waiting
            .unwrap()
            .contains("quiet")
    );
    let (terminal, _) = me(&ws, cx, worker);
    assert!(
        !call(
            handle,
            &ws,
            cx,
            &terminal,
            "reply",
            json!({"text": "[failed] The service rejected the approved request."})
        )
        .0
    );
    cx.run_until_parked();
    assert_eq!(
        cx.shown_system_notifications().len(),
        3,
        "failure reaches the background user"
    );
    assert_eq!(status(&ws, cx, board, card).status, QueueStatus::Failed);
}

#[gpui_kit::test]
fn expired_or_revoked_token_stops_pending_pickup(cx: &mut TestAppContext) {
    let (_data, handle, ws, worker, board, card, token) = setup(cx);
    queue(handle, &ws, cx, &token, card);
    ws.update(cx, |ws, _| {
        ws.api.store.as_mut().unwrap().revoke(token.id);
    });
    pickup(&ws, cx);
    assert_eq!(status(&ws, cx, board, card).status, QueueStatus::Blocked);
    assert!(ws.read_with(cx, |ws, _| {
        ws.space.terminal(worker).unwrap().handed.is_none()
    }));
}

#[gpui_kit::test]
fn queued_target_stays_in_its_workspace_and_current_token_folder_scope(cx: &mut TestAppContext) {
    let (_data, handle, ws, worker, board, card, _token) = setup(cx);
    let old_root = tempfile::tempdir().unwrap();
    let remaining_root = tempfile::tempdir().unwrap();
    let old_space = ws.read_with(cx, |ws, _| ws.space.id.clone());
    // Move the same live canvas to a second workspace, with the worker still using the first
    // workspace's folder. A token can initially use both folders, then loses the first workspace.
    let target_space = ws.update(cx, |ws, _| {
        ws.space.folder = Some(remaining_root.path().to_path_buf());
        ws.space.terminal_mut(worker).unwrap().seen.folder = Some(old_root.path().to_path_buf());
        ws.space.id = "queue-target".into();
        let old = super::super::Space {
            id: old_space.clone(),
            folder: Some(old_root.path().to_path_buf()),
            ..Default::default()
        };
        ws.others.push(old);
        ws.space.id.clone()
    });
    let limited = with_token(
        &ws,
        cx,
        Scope::Control,
        Some(vec![&old_space, &target_space]),
    );
    queue(handle, &ws, cx, &limited, card);
    ws.update(cx, |ws, cx| ws.api_forget_workspace(&old_space, cx));
    pickup(&ws, cx);
    let q = status(&ws, cx, board, card);
    assert_eq!(q.status, QueueStatus::Queued);
    assert!(q.waiting.unwrap().contains("authorized folders"));
    assert!(ws.read_with(cx, |ws, _| {
        ws.space.terminal(worker).unwrap().handed.is_none()
    }));
    ws.update(cx, |ws, _| {
        ws.space.terminal_mut(worker).unwrap().seen.folder =
            Some(remaining_root.path().to_path_buf())
    });
    pickup(&ws, cx);
    assert_eq!(status(&ws, cx, board, card).status, QueueStatus::Running);
}

#[gpui_kit::test]
fn inner_agent_must_obtain_approval_for_future_worker_even_for_its_own_card(
    cx: &mut TestAppContext,
) {
    let (_data, handle, ws, _worker, board, card, _token) = setup(cx);
    let workspace = ws.read_with(cx, |ws, _| ws.space.id.clone());
    ws.update(cx, |ws, _| {
        ws.templum_agent.conversation.own.cards.push(card)
    });
    let answers = super::templum_agent::control_later(
        handle,
        &ws,
        cx,
        901,
        "queue_card",
        json!({"card": card, "workspace": workspace}),
    );
    assert!(
        answers.try_recv().is_err(),
        "queued intent must wait for approval"
    );
    ws.read_with(cx, |ws, _| {
        assert!(ws.boards.card(board, card).unwrap().queue.is_none());
        assert_eq!(ws.templum_agent.approvals.len(), 1);
    });
}

#[gpui_kit::test]
fn enrolled_context_is_frozen_and_long_history_cannot_displace_confirmed_scope(
    cx: &mut TestAppContext,
) {
    let (_data, handle, ws, worker, board, card, token) = setup(cx);
    ws.update(cx, |ws, _| {
        ws.boards.edit_card(board, card, |c| {
            c.description = "CONFIRMED_SCOPE: edit only the local parser; verify its output.".into()
        });
        ws.boards
            .add_comment(board, card, "you", &"old history\n".repeat(30_000), 0);
    });
    queue(handle, &ws, cx, &token, card);
    ws.update(cx, |ws, _| {
        ws.boards.edit_card(board, card, |c| {
            c.title = "UNCONFIRMED_NEW_SCOPE".into();
            c.description = "Deploy an unrelated service.".into();
        });
        ws.boards.add_comment(
            board,
            card,
            "you",
            "UNCONFIRMED_NEW_SCOPE: ignore the old plan",
            0,
        );
    });
    pickup(&ws, cx);
    let q = status(&ws, cx, board, card);
    assert_eq!(q.status, QueueStatus::Running);
    assert!(q.context.contains("CONFIRMED_SCOPE"));
    assert!(!q.context.contains("UNCONFIRMED_NEW_SCOPE"));
    ws.read_with(cx, |ws, _| {
        let handed = ws.space.terminal(worker).unwrap().handed.as_ref().unwrap();
        assert!(handed.text.contains("CONFIRMED_SCOPE"), "{}", handed.text);
        assert!(!handed.text.contains("UNCONFIRMED_NEW_SCOPE"));
    });
}

#[gpui_kit::test]
fn native_manual_card_handoff_and_queue_admission_are_mutually_exclusive(cx: &mut TestAppContext) {
    let (_data, handle, ws, worker, board, card, token) = setup(cx);
    let (terminal, uid) = me(&ws, cx, worker);
    let workspace = ws.read_with(cx, |ws, _| ws.space.id.clone());
    let _receipt = super::api::api_later(
        handle,
        &ws,
        cx,
        &token,
        991,
        "tools/call",
        json!({"name": "delegate", "arguments": {"card": card, "block": uid, "task": "Confirmed manual work", "wait": false}}),
    );
    ws.read_with(cx, |ws, _| {
        assert_eq!(
            ws.pane(&terminal).unwrap().handed.as_ref().unwrap().card,
            Some(card)
        )
    });
    let refused = tool(
        handle,
        &ws,
        cx,
        &token,
        "queue_card",
        json!({"card": card, "workspace": workspace}),
    );
    assert!(said(&refused).0, "{refused}");
    assert!(ws.read_with(cx, |ws, _| {
        ws.boards.card(board, card).unwrap().queue.is_none()
    }));
    let duplicate = tool(
        handle,
        &ws,
        cx,
        &token,
        "delegate",
        json!({"card": card, "block": uid, "task": "Duplicate", "wait": false}),
    );
    assert!(said(&duplicate).0, "{duplicate}");
    assert!(
        !call(
            handle,
            &ws,
            cx,
            &terminal,
            "reply",
            json!({"text": "Manual task finished."})
        )
        .0
    );
    ws.update(cx, |ws, _| {
        ws.space.terminal_mut(worker).unwrap().sent = None
    });
    queue(handle, &ws, cx, &token, card);
    let refused = tool(
        handle,
        &ws,
        cx,
        &token,
        "delegate",
        json!({"card": card, "block": uid, "task": "Races with queued pickup", "wait": false}),
    );
    assert!(said(&refused).0, "{refused}");
    let (failed, _) = call(
        handle,
        &ws,
        cx,
        &terminal,
        "claim_card",
        json!({"card": card}),
    );
    assert!(failed, "manual claim must not silently adopt queued work");
    pickup(&ws, cx);
    assert_eq!(status(&ws, cx, board, card).status, QueueStatus::Running);
}

#[gpui_kit::test]
fn withdrawn_enrollment_can_be_rebound_but_never_runs_in_a_different_workspace_implicitly(
    cx: &mut TestAppContext,
) {
    let (_data, handle, ws, worker, board, card, token) = setup(cx);
    let first = ws.read_with(cx, |ws, _| ws.space.id.clone());
    let worker_uid = ws.read_with(cx, |ws, _| ws.space.uids[&worker].clone());
    queue(handle, &ws, cx, &token, card);
    let cancelled = tool(
        handle,
        &ws,
        cx,
        &token,
        "cancel_queued_card",
        json!({"card": card, "workspace": first}),
    );
    assert!(!said(&cancelled).0, "{cancelled}");
    cx.update_window(handle, |_, window, cx| {
        ws.update(cx, |ws, cx| {
            ws.new_space(window, cx);
            ws.add_kanban_at(Point::new(100.0, 100.0), window, cx);
            let block = ws.space.canvas.top().unwrap();
            ws.space.kanban_mut(block).unwrap().board = Some(board);
        })
    })
    .unwrap();
    queue(handle, &ws, cx, &token, card);
    pickup(&ws, cx);
    let target = ws.read_with(cx, |ws, _| ws.space.id.clone());
    assert_eq!(status(&ws, cx, board, card).workspace, target);
    assert_eq!(status(&ws, cx, board, card).status, QueueStatus::Queued);
    ws.read_with(cx, |ws, _| {
        let (space, block) = ws.find_block(&worker_uid).unwrap();
        assert_eq!(space, first);
        assert!(
            ws.space_named(&space)
                .unwrap()
                .terminal(block)
                .unwrap()
                .handed
                .is_none()
        );
    });
    // Native deletion withdraws even a global token's pending authorization before an id can
    // be reused. The original card is retained for an explicit new decision.
    cx.update_window(handle, |_, window, cx| {
        ws.update(cx, |ws, cx| ws.delete_space_as(&target, false, window, cx))
    })
    .unwrap();
    assert_eq!(status(&ws, cx, board, card).status, QueueStatus::Blocked);
    pickup(&ws, cx);
    assert_eq!(status(&ws, cx, board, card).status, QueueStatus::Blocked);
}

#[gpui_kit::test]
fn worker_without_an_explicit_reply_path_stays_queued_with_a_visible_reason(
    cx: &mut TestAppContext,
) {
    let (_data, handle, ws, worker, board, card, token) = setup(cx);
    ws.update(cx, |ws, _| {
        ws.space.terminal_mut(worker).unwrap().seen.agent = Some("aider".into())
    });
    queue(handle, &ws, cx, &token, card);
    pickup(&ws, cx);
    let q = status(&ws, cx, board, card);
    assert_eq!(q.status, QueueStatus::Queued);
    assert!(q.waiting.unwrap().contains("cannot explicitly report"));
    assert!(ws.read_with(cx, |ws, _| {
        ws.space.terminal(worker).unwrap().handed.is_none()
    }));
}
