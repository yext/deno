// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

use crate::args::Flags;
use crate::colors;
use crate::util::fs::canonicalize_path;

use deno_core::error::AnyError;
use deno_core::error::JsError;
use deno_core::futures::Future;
use deno_core::futures::FutureExt;
use deno_runtime::fmt_errors::format_js_error;
use log::info;
use notify::event::Event as NotifyEvent;
use notify::event::EventKind;
use notify::Error as NotifyError;
use notify::RecommendedWatcher;
use notify::RecursiveMode;
use notify::Watcher;
use std::collections::HashSet;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::select;
use tokio::sync::mpsc;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::sleep;

const CLEAR_SCREEN: &str = "\x1B[2J\x1B[1;1H";
const DEBOUNCE_INTERVAL: Duration = Duration::from_millis(200);

struct DebouncedReceiver {
  // The `recv()` call could be used in a tokio `select!` macro,
  // and so we store this state on the struct to ensure we don't
  // lose items if a `recv()` never completes
  received_items: HashSet<PathBuf>,
  receiver: UnboundedReceiver<Vec<PathBuf>>,
}

impl DebouncedReceiver {
  fn new_with_sender() -> (Arc<mpsc::UnboundedSender<Vec<PathBuf>>>, Self) {
    let (sender, receiver) = mpsc::unbounded_channel();
    (
      Arc::new(sender),
      Self {
        receiver,
        received_items: HashSet::new(),
      },
    )
  }

  async fn recv(&mut self) -> Option<Vec<PathBuf>> {
    if self.received_items.is_empty() {
      self
        .received_items
        .extend(self.receiver.recv().await?.into_iter());
    }

    loop {
      select! {
        items = self.receiver.recv() => {
          self.received_items.extend(items?);
        }
        _ = sleep(DEBOUNCE_INTERVAL) => {
          return Some(self.received_items.drain().collect());
        }
      }
    }
  }
}

async fn error_handler<F>(watch_future: F) -> bool
where
  F: Future<Output = Result<(), AnyError>>,
{
  let result = watch_future.await;
  if let Err(err) = result {
    let error_string = match err.downcast_ref::<JsError>() {
      Some(e) => format_js_error(e),
      None => format!("{err:?}"),
    };
    eprintln!(
      "{}: {}",
      colors::red_bold("error"),
      error_string.trim_start_matches("error: ")
    );
    false
  } else {
    true
  }
}

pub struct PrintConfig {
  /// printing watcher status to terminal.
  pub job_name: String,
  /// determine whether to clear the terminal screen; applicable to TTY environments only.
  pub clear_screen: bool,
}

fn create_print_after_restart_fn(clear_screen: bool) -> impl Fn() {
  move || {
    if clear_screen && std::io::stderr().is_terminal() {
      eprint!("{CLEAR_SCREEN}");
    }
    info!(
      "{} File change detected! Restarting!",
      colors::intense_blue("Watcher"),
    );
  }
}

/// An interface to interact with Deno's CLI file watcher.
#[derive(Debug)]
pub struct WatcherCommunicator {
  /// Send a list of paths that should be watched for changes.
  paths_to_watch_tx: tokio::sync::mpsc::UnboundedSender<Vec<PathBuf>>,

  /// Listen for a list of paths that were changed.
  changed_paths_rx: tokio::sync::broadcast::Receiver<Option<Vec<PathBuf>>>,

  /// Send a message to force a restart.
  restart_tx: tokio::sync::mpsc::UnboundedSender<()>,
}

impl Clone for WatcherCommunicator {
  fn clone(&self) -> Self {
    Self {
      paths_to_watch_tx: self.paths_to_watch_tx.clone(),
      changed_paths_rx: self.changed_paths_rx.resubscribe(),
      restart_tx: self.restart_tx.clone(),
    }
  }
}

impl WatcherCommunicator {
  pub fn watch_paths(&self, paths: Vec<PathBuf>) -> Result<(), AnyError> {
    self.paths_to_watch_tx.send(paths).map_err(AnyError::from)
  }
}

/// Creates a file watcher.
///
/// - `operation` is the actual operation we want to run every time the watcher detects file
/// changes. For example, in the case where we would like to bundle, then `operation` would
/// have the logic for it like bundling the code.
pub async fn watch_func<O, F>(
  flags: Flags,
  print_config: PrintConfig,
  operation: O,
) -> Result<(), AnyError>
where
  O: FnMut(
    Flags,
    WatcherCommunicator,
    Option<Vec<PathBuf>>,
  ) -> Result<F, AnyError>,
  F: Future<Output = Result<(), AnyError>>,
{
  let fut = watch_recv(
    flags,
    print_config,
    WatcherRestartMode::Automatic,
    operation,
  )
  .boxed_local();

  fut.await
}

#[derive(Clone, Copy, Debug)]
pub enum WatcherRestartMode {
  /// When a file path changes the process is restarted.
  Automatic,

  /// When a file path changes the caller will trigger a restart, using
  /// `WatcherCommunicator.restart_tx`.
  // TODO(bartlomieju): this mode will be used in a follow up PR
  #[allow(dead_code)]
  Manual,
}

/// Creates a file watcher.
///
/// - `operation` is the actual operation we want to run every time the watcher detects file
/// changes. For example, in the case where we would like to bundle, then `operation` would
/// have the logic for it like bundling the code.
pub async fn watch_recv<O, F>(
  mut flags: Flags,
  print_config: PrintConfig,
  restart_mode: WatcherRestartMode,
  mut operation: O,
) -> Result<(), AnyError>
where
  O: FnMut(
    Flags,
    WatcherCommunicator,
    Option<Vec<PathBuf>>,
  ) -> Result<F, AnyError>,
  F: Future<Output = Result<(), AnyError>>,
{
  let (paths_to_watch_tx, mut paths_to_watch_rx) =
    tokio::sync::mpsc::unbounded_channel();
  let (restart_tx, mut restart_rx) = tokio::sync::mpsc::unbounded_channel();
  let (changed_paths_tx, changed_paths_rx) = tokio::sync::broadcast::channel(4);
  let (watcher_sender, mut watcher_receiver) =
    DebouncedReceiver::new_with_sender();

  let PrintConfig {
    job_name,
    clear_screen,
  } = print_config;

  let print_after_restart = create_print_after_restart_fn(clear_screen);
  let watcher_communicator = WatcherCommunicator {
    paths_to_watch_tx: paths_to_watch_tx.clone(),
    changed_paths_rx: changed_paths_rx.resubscribe(),
    restart_tx: restart_tx.clone(),
  };
  info!("{} {} started.", colors::intense_blue("Watcher"), job_name,);

  let mut changed_paths = None;
  loop {
    // We may need to give the runtime a tick to settle, as cancellations may need to propagate
    // to tasks. We choose yielding 10 times to the runtime as a decent heuristic. If watch tests
    // start to fail, this may need to be increased.
    for _ in 0..10 {
      tokio::task::yield_now().await;
    }

    let mut watcher = new_watcher(watcher_sender.clone())?;
    consume_paths_to_watch(&mut watcher, &mut paths_to_watch_rx);

    let receiver_future = async {
      loop {
        let maybe_paths = paths_to_watch_rx.recv().await;
        add_paths_to_watcher(&mut watcher, &maybe_paths.unwrap());
      }
    };
    let operation_future = error_handler(operation(
      flags.clone(),
      watcher_communicator.clone(),
      changed_paths.take(),
    )?);

    // don't reload dependencies after the first run
    flags.reload = false;

    select! {
      _ = receiver_future => {},
      _ = restart_rx.recv() => {
        print_after_restart();
        continue;
      },
      received_changed_paths = watcher_receiver.recv() => {
        changed_paths = received_changed_paths.clone();

        match restart_mode {
          WatcherRestartMode::Automatic => {
            print_after_restart();
            continue;
          },
          WatcherRestartMode::Manual => {
            // TODO(bartlomieju): should we fail on sending changed paths?
            let _ = changed_paths_tx.send(received_changed_paths);
          }
        }
      },
      success = operation_future => {
        consume_paths_to_watch(&mut watcher, &mut paths_to_watch_rx);
        // TODO(bartlomieju): print exit code here?
        info!(
          "{} {} {}. Restarting on file change...",
          colors::intense_blue("Watcher"),
          job_name,
          if success {
            "finished"
          } else {
            "failed"
          }
        );
      },
    };

    let receiver_future = async {
      loop {
        let maybe_paths = paths_to_watch_rx.recv().await;
        add_paths_to_watcher(&mut watcher, &maybe_paths.unwrap());
      }
    };

    // If we got this far, it means that the `operation` has finished; let's wait
    // and see if there are any new paths to watch received or any of the already
    // watched paths has changed.
    select! {
      _ = receiver_future => {},
      received_changed_paths = watcher_receiver.recv() => {
        print_after_restart();
        changed_paths = received_changed_paths;
        continue;
      },
    };
  }
}

fn new_watcher(
  sender: Arc<mpsc::UnboundedSender<Vec<PathBuf>>>,
) -> Result<RecommendedWatcher, AnyError> {
  Ok(Watcher::new(
    move |res: Result<NotifyEvent, NotifyError>| {
      let Ok(event) = res else {
        return;
      };

      if !matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
      ) {
        return;
      }

      let paths = event
        .paths
        .iter()
        .filter_map(|path| canonicalize_path(path).ok())
        .collect();
      sender.send(paths).unwrap();
    },
    Default::default(),
  )?)
}

fn add_paths_to_watcher(watcher: &mut RecommendedWatcher, paths: &[PathBuf]) {
  // Ignore any error e.g. `PathNotFound`
  for path in paths {
    let _ = watcher.watch(path, RecursiveMode::Recursive);
  }
  log::debug!("Watching paths: {:?}", paths);
}

fn consume_paths_to_watch(
  watcher: &mut RecommendedWatcher,
  receiver: &mut UnboundedReceiver<Vec<PathBuf>>,
) {
  loop {
    match receiver.try_recv() {
      Ok(paths) => {
        add_paths_to_watcher(watcher, &paths);
      }
      Err(e) => match e {
        mpsc::error::TryRecvError::Empty => {
          break;
        }
        // there must be at least one receiver alive
        _ => unreachable!(),
      },
    }
  }
}
