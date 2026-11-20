use libloading::Library;
use serde_json::{json, Value};
use std::{
    error::Error,
    ffi::{CStr, CString},
    io::{self, BufRead},
    os::raw::{c_char, c_void},
    sync::mpsc::{self, TryRecvError},
    thread,
    time::Duration,
};

type Create = unsafe extern "C" fn() -> *mut c_void;
type Send = unsafe extern "C" fn(*mut c_void, *const c_char);
type Receive = unsafe extern "C" fn(*mut c_void, f64) -> *const c_char;
type Destroy = unsafe extern "C" fn(*mut c_void);

struct TdClient {
    // TDLib-клиент используется только в отдельном потоке.
    client: *mut c_void,
    send_fn: Send,
    receive_fn: Receive,
    destroy_fn: Destroy,
    // Библиотека должна оставаться загруженной, пока используются указатели на её функции.
    _library: Library,
}

// Безопасно при условии, что клиент TDLib используется только внутри worker-потока.
unsafe impl Send for TdClient {}

impl TdClient {
    fn new() -> Result<Self, Box<dyn Error>> {
        // Положите tdjson.dll рядом с исполняемым файлом.
        let library = unsafe { Library::new("tdjson.dll")? };

        let create: Create = unsafe { *library.get(b"td_json_client_create\0")? };
        let send_fn: Send = unsafe { *library.get(b"td_json_client_send\0")? };
        let receive_fn: Receive = unsafe { *library.get(b"td_json_client_receive\0")? };
        let destroy_fn: Destroy = unsafe { *library.get(b"td_json_client_destroy\0")? };

        let client = unsafe { create() };
        if client.is_null() {
            return Err("TDLib не смог создать клиент".into());
        }

        Ok(Self {
            client,
            send_fn,
            receive_fn,
            destroy_fn,
            _library: library,
        })
    }

    fn send(&self, request: &Value) -> Result<(), Box<dyn Error>> {
        let encoded = serde_json::to_string(request)?;
        let c_string = CString::new(encoded)?;
        unsafe {
            (self.send_fn)(self.client, c_string.as_ptr());
        }
        Ok(())
    }

    fn receive(&self) -> Option<Value> {
        let ptr = unsafe { (self.receive_fn)(self.client, 0.1) };
        if ptr.is_null() {
            return None;
        }

        // Ответ копируется в Rust String до следующего вызова receive().
        let text = unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned();
        serde_json::from_str(&text).ok()
    }
}

impl Drop for TdClient {
    fn drop(&mut self) {
        if !self.client.is_null() {
            unsafe {
                (self.destroy_fn)(self.client);
            }
        }
    }
}

fn handle_update(client: &TdClient, update: &Value, parameters_sent: &mut bool) -> bool {
    let kind = update["@type"].as_str().unwrap_or("");

    match kind {
        "updateAuthorizationState" => {
            let state = update["authorization_state"]["@type"]
                .as_str()
                .unwrap_or("unknown");

            println!("\nСостояние авторизации: {state}");

            match state {
                "authorizationStateWaitTdlibParameters" if !*parameters_sent => {
                    let api_id = match std::env::var("TELEGRAM_API_ID") {
                        Ok(value) => value,
                        Err(_) => {
                            eprintln!("Задайте TELEGRAM_API_ID и TELEGRAM_API_HASH в переменных среды.");
                            return false;
                        }
                    };
                    let api_hash = match std::env::var("TELEGRAM_API_HASH") {
                        Ok(value) => value,
                        Err(_) => {
                            eprintln!("Задайте TELEGRAM_API_ID и TELEGRAM_API_HASH в переменных среды.");
                            return false;
                        }
                    };

                    let api_id: i32 = match api_id.parse() {
                        Ok(value) => value,
                        Err(_) => {
                            eprintln!("TELEGRAM_API_ID должен быть числом.");
                            return false;
                        }
                    };

                    let request = json!({
                        "@type": "setTdlibParameters",
                        "database_directory": "tdlib-data",
                        "files_directory": "tdlib-files",
                        "use_file_database": true,
                        "use_chat_info_database": true,
                        "use_message_database": true,
                        "use_secret_chats": false,
                        "api_id": api_id,
                        "api_hash": api_hash,
                        "system_language_code": "en",
                        "device_model": "TelegramFast Rust",
                        "system_version": "Windows",
                        "application_version": "0.1.0",
                        "enable_storage_optimizer": true,
                        "ignore_file_names": false
                    });

                    if let Err(error) = client.send(&request) {
                        eprintln!("Не удалось настроить TDLib: {error}");
                        return false;
                    }
                    *parameters_sent = true;
                }
                "authorizationStateWaitEncryptionKey" => {
                    let _ = client.send(&json!({
                        "@type": "checkDatabaseEncryptionKey",
                        "encryption_key": ""
                    }));
                }
                "authorizationStateWaitPhoneNumber" => {
                    println!("Введите номер: /phone +15551234567");
                }
                "authorizationStateWaitCode" => {
                    println!("Введите код из Telegram: /code 12345");
                }
                "authorizationStateWaitPassword" => {
                    println!("Для аккаунта включён пароль 2FA: /password ваш_пароль");
                }
                "authorizationStateReady" => {
                    println!("Вход выполнен. Загружаю чаты…");
                    let _ = client.send(&json!({
                        "@type": "getChats",
                        "chat_list": { "@type": "chatListMain" },
                        "limit": 50
                    }));
                }
                "authorizationStateClosed" => {
                    println!("TDLib закрыла сессию.");
                }
                _ => {}
            }
        }

        "updateNewChat" => {
            let chat = &update["chat"];
            println!(
                "Чат: id={} | {}",
                chat["id"].as_i64().unwrap_or_default(),
                chat["title"].as_str().unwrap_or("(без названия)")
            );
        }

        "updateChatTitle" => {
            println!(
                "Новое название чата {}: {}",
                update["chat_id"].as_i64().unwrap_or_default(),
                update["title"].as_str().unwrap_or("")
            );
        }

        "updateNewMessage" => {
            let message = &update["message"];
            let chat_id = message["chat_id"].as_i64().unwrap_or_default();
            let text = message["content"]["text"]["text"]
                .as_str()
                .unwrap_or("[не текстовое сообщение]");

            println!("\n[{chat_id}] {text}");
        }

        "error" => {
            eprintln!(
                "Ошибка TDLib {}: {}",
                update["code"].as_i64().unwrap_or_default(),
                update["message"].as_str().unwrap_or("неизвестная ошибка")
            );
        }

        _ => {}
    }

    true
}

fn handle_command(client: &TdClient, command: &str) -> bool {
    if command == "/quit" {
        return false;
    }

    if let Some(phone) = command.strip_prefix("/phone ") {
        let _ = client.send(&json!({
            "@type": "setAuthenticationPhoneNumber",
            "phone_number": phone.trim(),
            "settings": { "@type": "phoneNumberAuthenticationSettings" }
        }));
    } else if let Some(code) = command.strip_prefix("/code ") {
        let _ = client.send(&json!({
            "@type": "checkAuthenticationCode",
            "code": code.trim()
        }));
    } else if let Some(password) = command.strip_prefix("/password ") {
        let _ = client.send(&json!({
            "@type": "checkAuthenticationPassword",
            "password": password.trim()
        }));
    } else if command == "/chats" {
        let _ = client.send(&json!({
            "@type": "getChats",
            "chat_list": { "@type": "chatListMain" },
            "limit": 50
        }));
    } else if let Some(rest) = command.strip_prefix("/send ") {
        match rest.split_once(' ') {
            Some((chat_id, text)) => match chat_id.parse::<i64>() {
                Ok(chat_id) => {
                    let _ = client.send(&json!({
                        "@type": "sendMessage",
                        "chat_id": chat_id,
                        "input_message_content": {
                            "@type": "inputMessageText",
                            "text": {
                                "@type": "formattedText",
                                "text": text,
                                "entities": []
                            }
                        }
                    }));
                }
                Err(_) => eprintln!("Формат: /send <chat_id> <текст>"),
            },
            None => eprintln!("Формат: /send <chat_id> <текст>"),
        }
    } else if !command.trim().is_empty() {
        println!("Команды: /phone, /code, /password, /chats, /send, /quit");
    }

    true
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("TelegramFast Rust — введите /quit для выхода.");

    let (command_tx, command_rx) = mpsc::channel::<String>();

    thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            match line {
                Ok(line) => {
                    let quit = line.trim() == "/quit";
                    if command_tx.send(line).is_err() || quit {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let client = TdClient::new()?;
    let mut parameters_sent = false;
    let mut running = true;

    while running {
        loop {
            match command_rx.try_recv() {
                Ok(command) => {
                    if !handle_command(&client, &command) {
                        running = false;
                        break;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    running = false;
                    break;
                }
            }
        }

        if let Some(update) = client.receive() {
            running = handle_update(&client, &update, &mut parameters_sent);
        } else {
            thread::sleep(Duration::from_millis(20));
        }
    }

    Ok(())
}