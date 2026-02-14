use anyhow::Result;

use crate::cli::{AnalyticsAction, AnalyticsCommand};
use crate::usage::{self, AnalyticsConsent};

pub fn run(cmd: AnalyticsCommand) -> Result<()> {
    match cmd.action.unwrap_or(AnalyticsAction::Status) {
        AnalyticsAction::Status => {
            let status = usage::status()?;
            println!("consent: {:?}", status.consent);
            println!("effective_enabled: {}", status.effective_enabled);
            println!("install_id: {}", status.install_id);
            println!("endpoint: {}", status.endpoint);
            println!("queue_path: {}", status.queue_path.display());
            println!("queued_events: {}", status.queued_events);
        }
        AnalyticsAction::Enable => {
            usage::set_consent(AnalyticsConsent::Enabled)?;
            println!("Anonymous usage tracking enabled.");
        }
        AnalyticsAction::Disable => {
            usage::set_consent(AnalyticsConsent::Disabled)?;
            println!("Anonymous usage tracking disabled.");
        }
        AnalyticsAction::Export => {
            let content = usage::export_queue()?;
            if content.trim().is_empty() {
                println!("(no queued analytics events)");
            } else {
                print!("{content}");
            }
        }
        AnalyticsAction::Purge => {
            usage::purge_queue()?;
            println!("Purged queued analytics events.");
        }
    }
    Ok(())
}
