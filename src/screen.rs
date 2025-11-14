use anyhow::{Context, Result};
use serde::Serialize;
use std::{
    fmt,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{RwLock, broadcast},
    time,
};

use crate::cli::ScreenOpts;

#[derive(Clone)]
pub struct ScreenBroadcaster {
    sender: broadcast::Sender<ScreenFrame>,
    latest: Arc<RwLock<Option<ScreenFrame>>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScreenFrame {
    pub frame_number: u64,
    pub captured_at_ms: u128,
    pub encoding: String,
    pub payload: String,
}

impl fmt::Display for ScreenFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "#{frame:<5} @ {ts}ms | {}",
            self.payload,
            frame = self.frame_number,
            ts = self.captured_at_ms
        )
    }
}

impl ScreenBroadcaster {
    pub fn with_mock_stream(buffer: usize, fps: u8) -> Self {
        let broadcaster = Self::new(buffer);
        broadcaster.spawn_mock_stream(fps);
        broadcaster
    }

    pub fn new(buffer: usize) -> Self {
        let (sender, _) = broadcast::channel(buffer);
        Self {
            sender,
            latest: Arc::new(RwLock::new(None)),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ScreenFrame> {
        self.sender.subscribe()
    }

    pub async fn latest(&self) -> Option<ScreenFrame> {
        self.latest.read().await.clone()
    }

    fn spawn_mock_stream(&self, fps: u8) {
        let fps = fps.max(1);
        let period = Duration::from_millis((1000 / fps as u64).max(1));
        let mut frame_number = 0_u64;
        let handle = self.clone();

        tokio::spawn(async move {
            let mut ticker = time::interval(period);
            loop {
                ticker.tick().await;
                frame_number += 1;
                let payload = build_ascii_frame(frame_number);
                let frame = ScreenFrame {
                    frame_number,
                    captured_at_ms: current_epoch_ms(),
                    encoding: "text/mock".to_string(),
                    payload,
                };

                handle.publish(frame).await;
            }
        });
    }

    async fn publish(&self, frame: ScreenFrame) {
        {
            let mut guard = self.latest.write().await;
            *guard = Some(frame.clone());
        }

        // Ignore lagging consumers; they'll resubscribe and catch up.
        let _ = self.sender.send(frame);
    }
}

pub async fn preview(opts: ScreenOpts) -> Result<()> {
    let generator = ScreenBroadcaster::with_mock_stream(opts.frame_buffer, opts.fps);
    let mut rx = generator.subscribe();

    for _ in 0..opts.frames {
        let frame = rx
            .recv()
            .await
            .context("screen preview channel closed unexpectedly")?;

        println!("{frame}");
    }

    Ok(())
}

fn current_epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|dur| dur.as_millis())
        .unwrap_or(0)
}

fn build_ascii_frame(frame: u64) -> String {
    const WIDTH: usize = 32;
    const FILL: char = '#';
    const EMPTY: char = '.';

    let position = (frame as usize) % WIDTH;
    let mut line = String::with_capacity(WIDTH);
    for idx in 0..WIDTH {
        if idx == position {
            line.push(FILL);
        } else {
            line.push(EMPTY);
        }
    }

    line
}
