//! Manual history benchmark fixture. Included only by the benchmark driver.
use chrono::Utc;
use speedrun_core::{EventPayload, MilestoneOrigin, category};

use crate::store::Store;

/// Construct a signed, gapless single-device run in one transaction. Seeding
/// bypasses service projection work so it does not dominate the measurement.
pub fn seed(store: &mut Store, count: usize) -> anyhow::Result<()> {
    let categories = category::builtin()?;
    let category = &categories[0];
    let at = Utc::now() - chrono::Duration::milliseconds(count as i64);
    let minimum = 1 + category.milestones.len() * 2;
    anyhow::ensure!(count >= minimum, "history must contain at least {minimum} events");
    store.transaction::<_, anyhow::Error>(|tx| {
        tx.insert(
            "benchmark-run",
            "start",
            at,
            EventPayload::RunStarted {
                project_id: "benchmark-project".into(),
                project_root: None,
                category_id: category.id.clone(),
                category_version: category.version,
                device_pubkey: Some(tx.device_pubkey()),
                start_head: None,
                start_head_committed_at: None,
                ruleset_digest: Some(category::digest(category)?),
                disclosure_commitment: None,
            },
        )?;
        for milestone in &category.milestones {
            let milestone_id = format!("{}@{}/{}", category.id, category.version, milestone.key);
            tx.insert(
                "benchmark-run",
                &format!("{}-proposed", milestone.key),
                at,
                EventPayload::MilestoneProposed {
                    milestone_id: milestone_id.clone(),
                    origin: MilestoneOrigin::Predefined,
                    key: Some(milestone.key.clone()),
                    title: milestone.title.clone(),
                    description: Some(milestone.description.clone()),
                    required_trust: milestone.required_trust,
                },
            )?;
            tx.insert(
                "benchmark-run",
                &format!("{}-accepted", milestone.key),
                at,
                EventPayload::MilestoneAccepted {
                    milestone_id,
                    required_trust: None,
                },
            )?;
        }
        // A compact mix of metadata-only prompts and paired agent turns,
        // spread across eight sessions with monotonically increasing times.
        for i in 0..(count - minimum) {
            let session_id = format!("session-{}", (i / 3) % 8);
            let payload = match i % 3 {
                0 => EventPayload::PromptSubmitted {
                    session_id,
                    host: "benchmark".into(),
                    model: Some("example-model".into()),
                    content_digest: String::new(),
                    length: 80,
                    redacted: false,
                },
                1 => EventPayload::AgentTurnStarted { session_id },
                _ => EventPayload::AgentTurnEnded { session_id },
            };
            tx.insert(
                "benchmark-run",
                &format!("history-{i}"),
                at + chrono::Duration::milliseconds(i as i64 + 1),
                payload,
            )?;
        }
        Ok(())
    })?;
    Ok(())
}
