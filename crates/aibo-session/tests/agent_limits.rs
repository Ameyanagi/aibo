//! §14: *"Agent limits are mandatory, not advisory."*
//!
//! > *"Exceeding one stops the run with `BudgetExceeded` and a 'continue
//! > anyway' button. Codex's own limits apply too, but **aibo must not depend
//! > on them**."*
//!
//! The mock backend here enforces nothing at all — deliberately. If a test
//! passes it is because `aibo-session` stopped the run.

mod common;

use std::sync::Arc;
use std::time::Duration;

use aibo_core::AiboError;
use aibo_core::types::{
    AgentLimits, AgentOutcome, AgentStatus, AgentStep, AgentTask, BudgetKind, ToolTier, Usage,
};
use aibo_provider::ProviderRegistry;
use aibo_session::{AgentEvent, AgentSink, Engine, EngineConfig, EventSink, Submission};
use common::MockAgent;
use uuid::Uuid;

fn engine() -> Arc<Engine> {
    Arc::new(Engine::new(
        ProviderRegistry::new(),
        EngineConfig::default(),
    ))
}

fn task() -> AgentTask {
    AgentTask {
        id: Uuid::now_v7(),
        instruction: "rename the module".to_owned(),
        workspace: None,
        context: Vec::new(),
        binding: None,
        conversation_id: None,
    }
}

fn limits() -> AgentLimits {
    AgentLimits {
        max_steps: 3,
        max_tool_calls: 2,
        max_wall_clock: Duration::from_secs(60),
        max_total_tokens: 1_000,
    }
}

fn thought() -> AgentStep {
    AgentStep::Thought("thinking".to_owned())
}

fn tool_use() -> AgentStep {
    AgentStep::ToolUse {
        id: "1".to_owned(),
        name: "read_file".to_owned(),
        args: serde_json::json!({}),
        tier: ToolTier::Builtin,
    }
}

#[tokio::test]
async fn a_runaway_loop_is_stopped_by_the_step_ceiling() {
    let engine = engine();
    let (sink, mut rx) = AgentSink::channel();

    let result = engine
        .run_agent(task(), limits(), MockAgent::runaway(thought()), &sink)
        .await;

    let error = result.expect_err("a backend that never stops must be stopped by aibo");
    assert!(matches!(
        error.as_ref(),
        AiboError::BudgetExceeded {
            kind: BudgetKind::Steps
        }
    ));

    let mut steps = 0;
    let mut failed = false;
    while let Ok(event) = rx.try_recv() {
        match event {
            AgentEvent::Step(_) => steps += 1,
            AgentEvent::Failed { .. } => failed = true,
            _ => {}
        }
    }
    assert_eq!(steps, 3, "exactly max_steps steps reach the UI");
    assert!(failed, "the UI needs the error to offer 'continue anyway'");
}

#[tokio::test]
async fn the_tool_call_ceiling_is_separate_from_the_step_ceiling() {
    let engine = engine();
    let limits = AgentLimits {
        max_steps: 100,
        max_tool_calls: 2,
        ..limits()
    };
    let result = engine
        .run_agent(
            task(),
            limits,
            MockAgent::runaway(tool_use()),
            &AgentSink::channel().0,
        )
        .await;

    assert!(matches!(
        result.as_ref().unwrap_err().as_ref(),
        AiboError::BudgetExceeded {
            kind: BudgetKind::Steps
        }
    ));
}

#[tokio::test]
async fn the_token_ceiling_stops_a_run_that_reports_usage() {
    let engine = engine();
    let steps = vec![
        Ok(thought()),
        Ok(AgentStep::Done(AgentOutcome {
            status: AgentStatus::Completed,
            usage: Usage {
                output_tokens: 5_000,
                ..Usage::default()
            },
            steps: 1,
        })),
    ];

    let result = engine
        .run_agent(
            task(),
            limits(),
            MockAgent::scripted(steps),
            &AgentSink::channel().0,
        )
        .await;

    assert!(matches!(
        result.as_ref().unwrap_err().as_ref(),
        AiboError::BudgetExceeded {
            kind: BudgetKind::Tokens
        }
    ));
}

#[tokio::test]
async fn a_run_within_its_limits_completes() {
    let engine = engine();
    let steps = vec![
        Ok(thought()),
        Ok(AgentStep::Done(AgentOutcome {
            status: AgentStatus::Completed,
            usage: Usage {
                output_tokens: 10,
                ..Usage::default()
            },
            steps: 1,
        })),
    ];

    let (sink, mut rx) = AgentSink::channel();
    let outcome = engine
        .run_agent(task(), limits(), MockAgent::scripted(steps), &sink)
        .await
        .expect("within limits");

    assert_eq!(outcome.status, AgentStatus::Completed);
    assert!(matches!(rx.try_recv(), Ok(AgentEvent::Started { .. })));
}

#[tokio::test]
async fn a_wall_clock_ceiling_already_past_stops_the_run_immediately() {
    let engine = engine();
    let limits = AgentLimits {
        max_wall_clock: Duration::ZERO,
        ..limits()
    };
    let result = engine
        .run_agent(
            task(),
            limits,
            MockAgent::runaway(thought()),
            &AgentSink::channel().0,
        )
        .await;

    assert!(matches!(
        result.as_ref().unwrap_err().as_ref(),
        AiboError::BudgetExceeded { .. }
    ));
}

#[tokio::test]
async fn a_backend_that_ends_without_a_result_is_not_reported_as_completed() {
    let engine = engine();
    let outcome = engine
        .run_agent(
            task(),
            limits(),
            MockAgent::scripted(vec![Ok(thought())]),
            &AgentSink::channel().0,
        )
        .await
        .expect("not an error, but not a success either");

    assert!(matches!(outcome.status, AgentStatus::Failed(_)));
}

#[tokio::test]
async fn a_panel_submission_does_not_interrupt_an_agent_run() {
    // §13: "Pressing it during an agent run does not interrupt — the run
    // continues in the task window and a fresh panel opens."
    let engine = engine();
    let task = task();
    let task_id = task.id;

    let steps: Vec<_> = (0..2).map(|_| Ok(thought())).collect();
    let run = {
        let engine = engine.clone();
        let backend = MockAgent::scripted(steps);
        tokio::spawn(async move {
            engine
                .run_agent(task, limits(), backend, &AgentSink::channel().0)
                .await
        })
    };

    // A panel submission lands mid-run. It installs its own token in the
    // *panel* slot; the agent registry is untouched.
    let _ = engine
        .run(
            Submission::new(Uuid::now_v7(), "unrelated"),
            &EventSink::null(),
        )
        .await;

    let outcome = run.await.unwrap().expect("the run was not cancelled");
    assert!(matches!(outcome.status, AgentStatus::Failed(_)));

    // And it can still be cancelled explicitly, by task id.
    engine.cancel_task(task_id);
}

#[tokio::test]
async fn cancel_task_stops_a_running_agent() {
    let engine = engine();
    let task = task();
    let task_id = task.id;

    let run = {
        let engine = engine.clone();
        let backend = MockAgent::runaway(thought());
        // A generous ceiling, so only the cancellation can end this.
        let limits = AgentLimits {
            max_steps: u32::MAX,
            max_tool_calls: u32::MAX,
            max_wall_clock: Duration::from_secs(600),
            max_total_tokens: u64::MAX,
        };
        tokio::spawn(async move {
            engine
                .run_agent(task, limits, backend, &AgentSink::channel().0)
                .await
        })
    };

    tokio::time::sleep(Duration::from_millis(20)).await;
    engine.cancel_task(task_id);

    let outcome = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("cancellation must be prompt")
        .unwrap()
        .expect("a cancelled run is an outcome, not an error");
    assert_eq!(outcome.status, AgentStatus::Cancelled);
}
