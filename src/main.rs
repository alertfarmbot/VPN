// В сборке для Windows не открываем консольное окно.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod backend;
mod config;
mod format;
mod session;
mod store;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Timer, TimerMode};

use backend::{Command, Emit, Event};

slint::include_modules!();

const USAGE: &str = "\
Ferrogram — лёгкий клиент Telegram

Использование: ferrogram [--demo | --demo-login]

  --demo         показать интерфейс на выдуманных чатах, без сети
  --demo-login   то же, но начиная с экрана входа
  -h, --help     эта справка

Переменные окружения:
  TG_API_ID, TG_API_HASH   ключи приложения с my.telegram.org
  FERROGRAM_HOME           каталог с сессией и настройками";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut demo = false;
    let mut demo_login = false;
    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "--demo" => demo = true,
            "--demo-login" => demo_login = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => {
                eprintln!("Неизвестный параметр: {other}\n\n{USAGE}");
                std::process::exit(2);
            }
        }
    }

    let dir = config::data_dir();
    let ui = AppWindow::new()?;
    if let Some(dark) = config::load_dark_theme(&dir) {
        ui.global::<Theme>().set_dark(dark);
    }

    let (commands, receiver) = tokio::sync::mpsc::unbounded_channel();
    store::install(store::Store::new(&ui, commands.clone(), dir.clone()));
    connect_callbacks(&ui);

    // События воркера применяются в UI-потоке.
    let emit: Emit = Arc::new(|event| {
        let _ = slint::invoke_from_event_loop(move || store::with(|store| store.apply(event)));
    });
    let worker = thread::Builder::new().name("network".into()).spawn(move || {
        let run = AssertUnwindSafe(|| {
            if demo || demo_login {
                backend::demo::run(receiver, emit.clone(), demo_login);
            } else {
                backend::telegram::run(receiver, emit.clone(), dir);
            }
        });
        if catch_unwind(run).is_err() {
            emit(Event::Fatal("Сетевой модуль аварийно завершился. Перезапустите приложение".into()));
        }
    })?;

    let minute = Timer::default();
    minute.start(TimerMode::Repeated, Duration::from_secs(60), || store::with(|store| store.tick()));

    ui.run()?;

    // Даём воркеру время корректно закрыть соединение.
    let _ = commands.send(Command::Quit);
    let deadline = Instant::now() + Duration::from_secs(2);
    while !worker.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    store::uninstall();
    Ok(())
}

fn connect_callbacks(ui: &AppWindow) {
    ui.on_submit_credentials(|id, hash| store::with(|store| store.submit_credentials(&id, &hash)));
    ui.on_submit_phone(|phone| store::with(|store| store.submit_phone(&phone)));
    ui.on_submit_code(|code| store::with(|store| store.submit_code(&code)));
    ui.on_submit_password(|password| store::with(|store| store.submit_password(&password)));
    ui.on_auth_back(|| store::with(|store| store.auth_back()));

    ui.on_dialog_clicked(|key| store::with(|store| store.open_chat(key)));
    ui.on_search_edited(|text| store::with(|store| store.search_edited(&text)));
    ui.on_dialogs_near_end(|| store::with(|store| store.load_more_dialogs()));
    ui.on_logout(|| store::with(|store| store.logout()));
    ui.on_theme_toggled(|dark| store::with(|store| store.theme_toggled(dark)));

    ui.on_send_message(|text| store::with(|store| store.send_message(&text)));
    ui.on_composer_edited(|| store::with(|store| store.composer_edited()));
    ui.on_need_older(|| store::with(|store| store.need_older()));
    ui.on_close_chat(|| store::with(|store| store.close_chat()));
    ui.on_retry_message(|id| store::with(|store| store.retry_message(id)));
    ui.on_chat_viewed(|| store::with(|store| store.chat_viewed()));
}
