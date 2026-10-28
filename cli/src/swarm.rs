//! Multi-agent Swarm / Council: 4-5 models collaborate, debate and solve
//! tasks concurrently to deliver faster, higher-quality results.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::task::JoinSet;

use crate::activity::{ActivityKind, ActivityLog};
use crate::config::Config;
use crate::runtime::Runtime;
use crate::session::Session;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmAgentRole {
    pub name: String,
    pub title: String,
    pub role_prompt: String,
    pub model_override: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SwarmEvent {
    pub agent: String,
    pub phase: String,
    pub text: String,
}

pub struct SwarmResult {
    pub summary: String,
    pub dialogue: Vec<(String, String)>,
}

/// Discovers or builds the team of 4-5 models for the swarm.
/// Uses configured swarm models or derives diverse models from available profiles.
pub fn resolve_swarm_team(cfg: &Config) -> Vec<SwarmAgentRole> {
    let mut available_models = Vec::new();

    // Primary model
    available_models.push(cfg.model.clone());

    // Pull from provider profiles if configured
    for profile in &cfg.provider_profiles {
        for m in &profile.models {
            let full_name = format!("{}/{}", profile.id, m);
            if !available_models.contains(&full_name) {
                available_models.push(full_name);
            }
        }
    }

    // Assign roles to up to 5 models
    let get_model = |idx: usize| -> Option<String> {
        if idx < available_models.len() {
            Some(available_models[idx].clone())
        } else {
            None // Falls back to default cfg.model
        }
    };

    vec![
        SwarmAgentRole {
            name: "Architect".into(),
            title: "System & Architecture Planner".into(),
            role_prompt: "You are the Architect. Analyze high-level requirements, dependencies, edge cases, and architectural strategy.".into(),
            model_override: get_model(0),
        },
        SwarmAgentRole {
            name: "Researcher".into(),
            title: "Codebase & Pattern Explorer".into(),
            role_prompt: "You are the Researcher. Inspect patterns, related code structures, best practices, and potential bottlenecks.".into(),
            model_override: get_model(1),
        },
        SwarmAgentRole {
            name: "Coder".into(),
            title: "Implementation Specialist".into(),
            role_prompt: "You are the Lead Coder. Provide concrete, efficient, minimal, and correct implementations or code changes.".into(),
            model_override: get_model(2),
        },
        SwarmAgentRole {
            name: "Reviewer".into(),
            title: "Security & Quality Critic".into(),
            role_prompt: "You are the Reviewer. Rigorously critique the proposed solutions for bugs, regressions, security risks, or speed bottlenecks.".into(),
            model_override: get_model(3),
        },
        SwarmAgentRole {
            name: "Synthesizer".into(),
            title: "Council Consensus & Lead Exec".into(),
            role_prompt: "You are the Synthesizer. Review the inputs of all other models and produce the definitive, unified, optimal solution.".into(),
            model_override: get_model(4),
        },
    ]
}

/// Runs a concurrent multi-agent swarm discussion and problem-solving session.
pub async fn run_swarm<F>(
    cfg: &Config,
    cwd: &Path,
    task: &str,
    mut on_event: F,
) -> Result<SwarmResult>
where
    F: FnMut(SwarmEvent) + Send + 'static,
{
    let activity_id =
        ActivityLog::start(ActivityKind::Subagent, "swarm council".to_string(), task).ok();
    let team = resolve_swarm_team(cfg);
    let mut dialogue: Vec<(String, String)> = Vec::new();

    on_event(SwarmEvent {
        agent: "Council".into(),
        phase: "Init".into(),
        text: format!("Initiating 5-agent council across models to accelerate task: \"{task}\""),
    });

    // Phase 1: Parallel exploration and proposal (Architect, Researcher, Coder)
    let phase1_agents = vec![&team[0], &team[1], &team[2]];
    let mut set: JoinSet<Result<(String, String)>> = JoinSet::new();

    for agent in phase1_agents {
        let mut agent_cfg = cfg.clone();
        if let Some(m) = &agent.model_override {
            let _ = agent_cfg.select_model(m);
        }
        let cwd_buf = cwd.to_path_buf();
        let agent_name = agent.name.clone();
        let role_prompt = agent.role_prompt.clone();
        let task_str = task.to_string();

        set.spawn(async move {
            let mut rt = Runtime::new(agent_cfg.clone(), cwd_buf.clone())?;
            rt.set_notification_role(crate::notify::Role::Subagent(agent_name.clone()));
            let mut session = Session::new(&cwd_buf.display().to_string(), &agent_cfg.model)?;
            let prompt = format!(
                "{}\n\nTask:\n{}\n\nProvide your immediate domain assessment and concrete recommendation in 2-4 concise paragraphs.",
                role_prompt, task_str
            );
            let reply = rt.turn(&mut session, &prompt, |_| {}).await?;
            Ok((agent_name, reply))
        });
    }

    let mut blackboard = String::new();

    while let Some(res) = set.join_next().await {
        match res {
            Ok(Ok((name, text))) => {
                on_event(SwarmEvent {
                    agent: name.clone(),
                    phase: "Analysis".into(),
                    text: text.clone(),
                });
                blackboard.push_str(&format!("### {}'s input:\n{}\n\n", name, text));
                dialogue.push((name, text));
            }
            Ok(Err(e)) => {
                tracing::warn!("Swarm worker failed: {e}");
            }
            Err(e) => {
                tracing::warn!("Swarm task panic: {e}");
            }
        }
    }

    // Phase 2: Review & Critique (Reviewer inspects the blackboard)
    let reviewer = &team[3];
    let mut reviewer_cfg = cfg.clone();
    if let Some(m) = &reviewer.model_override {
        let _ = reviewer_cfg.select_model(m);
    }
    let cwd_buf = cwd.to_path_buf();
    let reviewer_name = reviewer.name.clone();
    let reviewer_role = reviewer.role_prompt.clone();
    let blackboard_clone = blackboard.clone();
    let task_str = task.to_string();

    let reviewer_result = tokio::spawn(async move {
        let mut rt = Runtime::new(reviewer_cfg.clone(), cwd_buf.clone())?;
        rt.set_notification_role(crate::notify::Role::Subagent(reviewer_name.clone()));
        let mut session = Session::new(&cwd_buf.display().to_string(), &reviewer_cfg.model)?;
        let prompt = format!(
            "{}\n\nOriginal Task:\n{}\n\nCouncil inputs so far:\n{}\n\nCritique these inputs. Highlight potential flaws, missed optimizations, or missing edge cases.",
            reviewer_role, task_str, blackboard_clone
        );
        let reply = rt.turn(&mut session, &prompt, |_| {}).await?;
        Ok::<(String, String), anyhow::Error>((reviewer_name, reply))
    }).await;

    if let Ok(Ok((name, text))) = reviewer_result {
        on_event(SwarmEvent {
            agent: name.clone(),
            phase: "Review".into(),
            text: text.clone(),
        });
        blackboard.push_str(&format!("### {}'s critique:\n{}\n\n", name, text));
        dialogue.push((name, text));
    }

    // Phase 3: Consensus & Synthesis (Synthesizer merges into ultimate solution)
    let synth = &team[4];
    let mut synth_cfg = cfg.clone();
    if let Some(m) = &synth.model_override {
        let _ = synth_cfg.select_model(m);
    }
    let cwd_buf = cwd.to_path_buf();
    let synth_name = synth.name.clone();
    let synth_role = synth.role_prompt.clone();
    let task_str = task.to_string();

    let synth_result = tokio::spawn(async move {
        let mut rt = Runtime::new(synth_cfg.clone(), cwd_buf.clone())?;
        rt.set_notification_role(crate::notify::Role::Subagent(synth_name.clone()));
        let mut session = Session::new(&cwd_buf.display().to_string(), &synth_cfg.model)?;
        let prompt = format!(
            "{}\n\nOriginal Task:\n{}\n\nFull Council Discussion:\n{}\n\nSynthesize the final, verified, optimal solution addressing all points raised.",
            synth_role, task_str, blackboard
        );
        let reply = rt.turn(&mut session, &prompt, |_| {}).await?;
        Ok::<(String, String), anyhow::Error>((synth_name, reply))
    }).await;

    let final_summary = match synth_result {
        Ok(Ok((name, text))) => {
            on_event(SwarmEvent {
                agent: name.clone(),
                phase: "Consensus".into(),
                text: text.clone(),
            });
            dialogue.push((name, text.clone()));
            text
        }
        _ => "Council synthesis encountered an error.".into(),
    };

    if let Some(id) = activity_id {
        let _ = ActivityLog::finish(&id, true, "swarm council completed");
    }

    Ok(SwarmResult {
        summary: final_summary,
        dialogue,
    })
}
