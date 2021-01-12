use crate::app::{App, FocusedPanel};
use crate::command;
use crate::parse::{self, Row};
use crate::template;
use crate::terminal_manager::TerminalManager;
use crate::ui;

use std::error::Error;

use crossterm::event::{self, Event as CEvent, KeyCode, KeyEvent, KeyModifiers};
use std::{
  sync::mpsc::{self, Receiver, Sender},
  thread,
  time::{Duration, Instant},
};

enum Event<I> {
  Input(I),
  Tick,
  RefetchData,
  RowsLoaded(Vec<Row>),
}

pub fn run(mut app: App) -> Result<(), Box<dyn Error>> {
  // select the first row (no rows will be loaded at this point but that's okay)
  app.table.next();

  let lines_to_skip = match app.args.lines_to_skip {
    0 => match app.profile {
      Some(profile) => profile.lines_to_skip,
      None => 0,
    },
    _ => app.args.lines_to_skip,
  };

  let mut terminal_manager = TerminalManager::new()?;

  let (tx, rx) = mpsc::channel();
  let (loading_tx, loading_rx) = mpsc::channel();

  poll_events(&tx);
  poll_loading(&tx, loading_rx);

  tx.send(Event::RefetchData).unwrap();

  loop {
    terminal_manager
      .terminal
      .draw(|frame| ui::draw(frame, &mut app))?;

    let mut on_event = |event: Event<KeyEvent>| -> Result<bool, Box<dyn Error>> {
      handle_event(
        event,
        &mut app,
        &mut terminal_manager,
        &tx,
        lines_to_skip,
        &loading_tx,
      )
    };

    // You might be wondering, what's going on here? As it so happens, we're blocking until the first event is received, and then processing any other events in the buffer before continuing. If we only handle one event per iteration of the loop, that's a lot of unnecessary drawing. On the other hand, if we don't block on any events, we'll end up drawing constantly while waiting for the next event to be received, causing CPU to go through the roof.
    if !on_event(rx.recv()?)? {
      break;
    }

    for backlogged_event in rx.try_iter() {
      if !on_event(backlogged_event)? {
        break;
      }
    }
  }

  Ok(())
}

fn get_rows_from_command(command: &str, skip_lines: usize) -> Vec<Row> {
  let output = command::run_command(command).unwrap();

  let trimmed_output = output
    .lines()
    .skip(skip_lines)
    .collect::<Vec<&str>>()
    .join("\n");

  parse::parse(trimmed_output)
}

fn poll_events(tx: &Sender<Event<KeyEvent>>) {
  let tick_rate = Duration::from_millis(10000); // TODO: do we really need this?
  let tx_clone = tx.clone();

  thread::spawn(move || {
    let mut last_tick = Instant::now();
    loop {
      // poll for tick rate duration, if no events, sent tick event.
      let timeout = tick_rate
        .checked_sub(last_tick.elapsed())
        .unwrap_or_else(|| Duration::from_secs(0));
      if event::poll(timeout).unwrap() {
        if let CEvent::Key(key) = event::read().unwrap() {
          tx_clone.send(Event::Input(key)).unwrap();
        }
      }
      if last_tick.elapsed() >= tick_rate {
        tx_clone.send(Event::Tick).unwrap();
        last_tick = Instant::now();
      }
    }
  });
}

fn poll_loading(tx: &Sender<Event<KeyEvent>>, loading_rx: Receiver<bool>) {
  let tx_clone = tx.clone();

  thread::spawn(move || {
    let interval = Duration::from_millis(100);
    let mut is_loading = false;

    loop {
      thread::sleep(interval);

      is_loading = if is_loading {
        match loading_rx.try_recv() {
          Ok(v) => v,
          Err(mpsc::TryRecvError::Empty) => is_loading,
          Err(e) => panic!("Unexpected error: {:?}", e),
        }
      } else {
        match loading_rx.recv() {
          Ok(v) => v,
          // we get this error when we quit the application so we're just returning false for now
          Err(_) => false,
        }
      };

      if is_loading {
        tx_clone.send(Event::Tick).unwrap();
      }
    }
  });
}

fn handle_event(
  event: Event<KeyEvent>,
  app: &mut App,
  terminal_manager: &mut TerminalManager,
  tx: &Sender<Event<KeyEvent>>,
  lines_to_skip: usize,
  loading_tx: &Sender<bool>,
) -> Result<bool, Box<dyn Error>> {
  match event {
    Event::Input(event) => {
      if event.code == KeyCode::Char('c') && event.modifiers == KeyModifiers::CONTROL {
        terminal_manager.teardown()?;
        return Ok(false);
      }

      match app.focused_panel {
        FocusedPanel::Table => match event.code {
          KeyCode::Char('q') => {
            terminal_manager.teardown()?;
            return Ok(false);
          }
          KeyCode::Esc => {
            app.reset_filter_text();
          }
          KeyCode::Down | KeyCode::Char('k') => {
            app.table.next();
            app.on_select();
          }
          KeyCode::Up | KeyCode::Char('j') => {
            app.table.previous();
            app.on_select();
          }
          KeyCode::Char('/') => {
            app.focused_panel = FocusedPanel::Search;
          }
          KeyCode::Char(c) => {
            match app.get_selected_row() {
              Some(selected_row) => {
                if app.profile.is_some() {
                  let binding = app
                    .profile
                    .unwrap()
                    .key_bindings
                    .iter()
                    .find(|&kb| kb.key == c);

                  if binding.is_some() {
                    let command = template::resolve_command(binding.unwrap(), selected_row);

                    app.status_text = Some(format!("Running command: {}", command));
                    loading_tx.send(true).unwrap();

                    let tx_clone = tx.clone();
                    thread::spawn(move || {
                      // TODO: don't just unwrap here
                      command::run_command(&command).unwrap();

                      tx_clone.send(Event::RefetchData).unwrap()
                    });
                  }
                }
              }
              None => (),
            }
          }
          _ => (),
        },
        FocusedPanel::Search => match event.code {
          KeyCode::Backspace => {
            app.pop_filter_text_char();
          }
          KeyCode::Esc => {
            app.reset_filter_text();
            app.focused_panel = FocusedPanel::Table;
          }
          KeyCode::Enter => {
            app.focused_panel = FocusedPanel::Table;
          }
          KeyCode::Char(c) => {
            app.push_filter_text_char(c);
          }
          _ => (),
        },
      }
    }
    Event::Tick => {
      app.on_tick();
    }
    Event::RefetchData => {
      refetch_data(app, tx, lines_to_skip, loading_tx);
    }
    Event::RowsLoaded(rows) => {
      on_rows_loaded(app, loading_tx, rows);
    }
  }

  Ok(true)
}

fn refetch_data(
  app: &mut App,
  tx: &Sender<Event<KeyEvent>>,
  lines_to_skip: usize,
  loading_tx: &Sender<bool>,
) {
  let command = app.args.command.clone();
  app.status_text = Some(format!("Running command: {} (if this is taking a while the program might be continuously streaming data which is not yet supported)", command));
  loading_tx.send(true).unwrap();

  let tx_clone = tx.clone();
  thread::spawn(move || {
    let rows = get_rows_from_command(&command, lines_to_skip);

    tx_clone.send(Event::RowsLoaded(rows)).unwrap()
  });
}

fn on_rows_loaded(app: &mut App, loading_tx: &Sender<bool>, rows: Vec<Row>) {
  app.update_rows(rows);

  app.status_text = None;
  loading_tx.send(false).unwrap();
}
