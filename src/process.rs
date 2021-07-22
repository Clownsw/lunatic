use std::future::Future;

use anyhow::Result;
use log::debug;
use tokio::{
    sync::mpsc::{channel, Sender, UnboundedSender},
    task::JoinHandle,
};
use wasmtime::Val;

use crate::message::Message;

#[derive(Debug)]
pub enum Signal {
    Kill,
}

/// The reason of a process finishing
pub enum Finished<T> {
    /// The Wasm function finished or trapped
    Wasm(T),
    /// The process was terminated by an external signal
    Signal(Signal),
}

/// The only way of communicating with processes is through a `ProcessHandle`.
///
/// Lunatic processes can be crated from a Wasm module & exported function name (or table index).
/// They are created inside the `Environment::spawn` method, and once spawned they will be running
/// in the background and can't be observed directly.
#[derive(Debug)]
pub struct ProcessHandle {
    signal_sender: Sender<Signal>,
    mailbox_sender: UnboundedSender<Message>,
    pub task: JoinHandle<()>,
}

impl ProcessHandle {
    /// Turns a Future into a process, enabling signals (e.g. kill) and messages.  
    pub(crate) fn new<F>(fut: F, mailbox_sender: UnboundedSender<Message>) -> Self
    where
        F: Future<Output = Result<Box<[Val]>>> + Send + 'static,
    {
        let (signal_sender, mut signal_mailbox) = channel::<Signal>(1);
        let fut = async move {
            tokio::pin!(fut);

            let mut disable_signals = false;
            let result = loop {
                tokio::select! {
                    biased;
                    // Handle signals first
                    signal = signal_mailbox.recv(), if !disable_signals => {
                        match signal {
                            // Exit loop and don't poll anymore the future if Signal::Kill received.
                            Some(Signal::Kill) => break Finished::Signal(Signal::Kill),
                            // Can't receive anymore signals, disable this `select!` branch
                            None => disable_signals = true
                        }
                    }
                    // Run process
                    output = &mut fut => { break Finished::Wasm(output); }
                }
            };
            match result {
                Finished::Wasm(Result::Err(err)) => debug!("Process failed: {}", err),
                _ => (),
            }
        };

        // Spawn a background process
        let task = tokio::spawn(fut);

        Self {
            signal_sender,
            mailbox_sender,
            task,
        }
    }

    // Send message to process
    pub fn send_message(&self, message: Message) -> Result<()> {
        Ok(self.mailbox_sender.send(message)?)
    }

    // Send signal to process
    pub async fn send_signal(&self, signal: Signal) -> Result<()> {
        Ok(self.signal_sender.send(signal).await?)
    }
}
