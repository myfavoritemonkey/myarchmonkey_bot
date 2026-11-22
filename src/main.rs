use serde::{Deserialize, Serialize};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::task::JoinHandle;

type BotError = Box<dyn std::error::Error + Send + Sync>;

const TELEGRAM_TEXT_CHUNK: usize = 3500;
const MAX_FILES_PER_TASK: usize = 10;
const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;
const MAX_HISTORY_TURNS: usize = 8;
const MAX_HISTORY_SIDE_CHARS: usize = 3000;
const MAX_INPUT_FILES: usize = 10;
const PROJECT_PAGE_SIZE: usize = 8;
const DEFAULT_MODEL: &str = "gpt-6.2-sol";
const MODELS: [(&str, &str); 10] = [
    ("gpt-6.2-sol", "GPT-6.2 Sol"),
    ("gpt-6.2-astra", "GPT-6.2 Astra"),
    ("gpt-6.1-terra", "GPT-6.1 Terra"),
    ("gpt-6.1-sol", "GPT-6.1 Sol"),
    ("gpt-6-astra", "GPT-6 Astra"),
    ("gpt-6-sol", "GPT-6 Sol"),
    ("gpt-6-luna", "GPT-6 Luna ⚡"),
    ("gpt-5.6-sol", "GPT-5.6 Sol"),
    ("gpt-5.6-terra", "GPT-5.6 Terra"),
    ("gpt-5.6-luna", "GPT-5.6 Luna"),
];

#[derive(Clone, Debug, Deserialize)]
struct Config {
    bot_token: String,
    allowed_user_id: i64,
    working_directory: PathBuf,
    codex_path: PathBuf,
    task_timeout_seconds: u64,
}

impl Config {
    fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let config: Self = toml::from_str(&std::fs::read_to_string(path)?)?;
        if config.bot_token == "PASTE_BOTFATHER_TOKEN_HERE" || config.bot_token.trim().is_empty() {
            return Err("укажите bot_token в config.toml".into());
        }
        if config.allowed_user_id <= 0 {
            return Err("укажите положительный allowed_user_id в config.toml".into());
        }
        if !config.working_directory.is_dir() {
            return Err("working_directory не существует или не является каталогом".into());
        }
        if !config.codex_path.is_file() {
            return Err("codex_path не существует или не является файлом".into());
        }
        if config.task_timeout_seconds == 0 {
            return Err("task_timeout_seconds должен быть больше нуля".into());
        }
        Ok(config)
    }
}

#[derive(Debug, Deserialize)]
struct TelegramResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Update {
    update_id: i64,
    message: Option<Message>,
    callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    id: String,
    from: User,
    message: Option<Message>,
    data: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Message {
    message_id: i64,
    chat: Chat,
    from: Option<User>,
    text: Option<String>,
    caption: Option<String>,
    document: Option<TelegramDocument>,
    photo: Option<Vec<TelegramPhoto>>,
}

#[derive(Debug, Deserialize)]
struct TelegramDocument {
    file_id: String,
    file_name: Option<String>,
    file_size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TelegramPhoto {
    file_id: String,
    file_size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Chat {
    id: i64,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Deserialize)]
struct User {
    id: i64,
}

#[derive(Serialize)]
struct GetUpdates {
    offset: i64,
    timeout: u32,
    allowed_updates: [&'static str; 2],
}

#[derive(Serialize)]
struct SendMessage<'a> {
    chat_id: i64,
    text: &'a str,
    reply_to_message_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_markup: Option<InlineKeyboardMarkup>,
}

#[derive(Serialize)]
struct InlineKeyboardMarkup {
    inline_keyboard: Vec<Vec<InlineKeyboardButton>>,
}

#[derive(Serialize)]
struct InlineKeyboardButton {
    text: String,
    callback_data: String,
}

#[derive(Serialize)]
struct AnswerCallbackQuery<'a> {
    callback_query_id: &'a str,
    text: &'a str,
}

#[derive(Serialize)]
struct GetFile<'a> {
    file_id: &'a str,
}

#[derive(Serialize)]
struct SetCommands {
    commands: Vec<BotCommand>,
    scope: BotCommandScope,
}

#[derive(Serialize)]
struct BotCommand {
    command: &'static str,
    description: &'static str,
}

#[derive(Serialize)]
struct BotCommandScope {
    #[serde(rename = "type")]
    kind: &'static str,
    chat_id: i64,
}

#[derive(Deserialize)]
struct TelegramFileInfo {
    file_path: Option<String>,
    file_size: Option<u64>,
}

#[derive(Clone, Deserialize, Serialize)]
struct PendingFile {
    file_id: String,
    name: String,
    size: Option<u64>,
}

#[derive(Clone, Deserialize, Serialize)]
struct State {
    next_offset: i64,
    #[serde(default = "default_model")]
    selected_model: String,
    #[serde(default)]
    history: Vec<ConversationTurn>,
    #[serde(default)]
    selected_project: Option<PathBuf>,
    #[serde(default)]
    pending_files: Vec<PendingFile>,
    #[serde(default)]
    context_generation: u64,
}

#[derive(Clone, Deserialize, Serialize)]
struct ConversationTurn {
    user: String,
    assistant: String,
}

fn default_model() -> String {
    DEFAULT_MODEL.to_string()
}

impl Default for State {
    fn default() -> Self {
        Self {
            next_offset: 0,
            selected_model: default_model(),
            history: Vec::new(),
            selected_project: None,
            pending_files: Vec::new(),
            context_generation: 0,
        }
    }
}

impl State {
    fn model_slug(&self) -> &str {
        if model_name(&self.selected_model).is_some() {
            &self.selected_model
        } else {
            DEFAULT_MODEL
        }
    }

    fn remember(&mut self, user: &str, assistant: &str) {
        self.history.push(ConversationTurn {
            user: user.chars().take(MAX_HISTORY_SIDE_CHARS).collect(),
            assistant: assistant.chars().take(MAX_HISTORY_SIDE_CHARS).collect(),
        });
        if self.history.len() > MAX_HISTORY_TURNS {
            self.history.drain(..self.history.len() - MAX_HISTORY_TURNS);
        }
    }

    fn project<'a>(&'a self, config: &'a Config) -> &'a Path {
        self.selected_project
            .as_deref()
            .filter(|path| path.is_dir())
            .unwrap_or(&config.working_directory)
    }
}

struct Browser {
    path: PathBuf,
    page: usize,
    nonce: i64,
}

struct TaskSpec {
    update_id: i64,
    reply_to_message_id: i64,
    prompt: String,
    files: Vec<PendingFile>,
    model: String,
    project: PathBuf,
    history: Vec<ConversationTurn>,
    context_generation: u64,
}

struct TaskOutcome {
    prompt: String,
    answer: Option<String>,
    context_generation: u64,
}

struct ActiveTask {
    handle: JoinHandle<TaskOutcome>,
    started: Instant,
    update_id: i64,
    model: String,
    project: PathBuf,
    reply_to_message_id: i64,
}

fn model_name(slug: &str) -> Option<&'static str> {
    MODELS
        .iter()
        .find(|(id, _)| *id == slug)
        .map(|(_, name)| *name)
}

fn is_authorized(message: &Message, user_id: i64) -> bool {
    message.chat.kind == "private"
        && message.chat.id == user_id
        && message.from.as_ref().is_some_and(|user| user.id == user_id)
}

fn is_authorized_callback(query: &CallbackQuery, user_id: i64) -> bool {
    query.from.id == user_id
        && query
            .message
            .as_ref()
            .is_some_and(|message| message.chat.kind == "private" && message.chat.id == user_id)
}

async fn telegram_post<T: Serialize, R: for<'de> Deserialize<'de>>(
    client: &reqwest::Client,
    token: &str,
    method: &str,
    body: &T,
) -> Result<R, BotError> {
    let url = format!("https://api.telegram.org/bot{token}/{method}");
    let response = client
        .post(url)
        .json(body)
        .send()
        .await
        .map_err(|error| format!("Ошибка сети Telegram: {}", error.without_url()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("Telegram API вернул HTTP {status}").into());
    }
    let payload: TelegramResponse<R> = response
        .json()
        .await
        .map_err(|error| format!("Ошибка ответа Telegram: {}", error.without_url()))?;
    if !payload.ok {
        return Err(payload
            .description
            .unwrap_or_else(|| "ошибка Telegram API".into())
            .into());
    }
    payload
        .result
        .ok_or_else(|| "Telegram API вернул пустой result".into())
}

async fn send_text(
    client: &reqwest::Client,
    config: &Config,
    chat_id: i64,
    reply_to_message_id: i64,
    text: &str,
) -> Result<(), BotError> {
    for chunk in split_text(text, TELEGRAM_TEXT_CHUNK) {
        let _: serde_json::Value = telegram_post(
            client,
            &config.bot_token,
            "sendMessage",
            &SendMessage {
                chat_id,
                text: &chunk,
                reply_to_message_id,
                reply_markup: None,
            },
        )
        .await?;
    }
    Ok(())
}

async fn send_model_menu(
    client: &reqwest::Client,
    config: &Config,
    chat_id: i64,
    reply_to_message_id: i64,
    selected_model: &str,
) -> Result<(), BotError> {
    let inline_keyboard = MODELS
        .chunks(2)
        .map(|pair| {
            pair.iter()
                .map(|(slug, name)| InlineKeyboardButton {
                    text: if *slug == selected_model {
                        format!("✓ {name}")
                    } else {
                        name.to_string()
                    },
                    callback_data: format!("model:{slug}"),
                })
                .collect()
        })
        .collect();
    let _: serde_json::Value = telegram_post(
        client,
        &config.bot_token,
        "sendMessage",
        &SendMessage {
            chat_id,
            text: "Выберите модель для следующих заданий:",
            reply_to_message_id,
            reply_markup: Some(InlineKeyboardMarkup { inline_keyboard }),
        },
    )
    .await?;
    Ok(())
}

#[derive(Serialize)]
struct EditMessageText<'a> {
    chat_id: i64,
    message_id: i64,
    text: &'a str,
    reply_markup: InlineKeyboardMarkup,
}

fn subdirectories(path: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut directories = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            directories.push(entry.path());
        }
    }
    directories.sort_by_key(|path| path.file_name().map(|name| name.to_ascii_lowercase()));
    Ok(directories)
}

fn project_menu(
    browser: &Browser,
    selected: &Path,
) -> std::io::Result<(String, InlineKeyboardMarkup)> {
    let directories = subdirectories(&browser.path)?;
    let total_pages = directories.len().max(1).div_ceil(PROJECT_PAGE_SIZE);
    let page = browser.page.min(total_pages - 1);
    let mut rows = vec![vec![InlineKeyboardButton {
        text: "✅ Выбрать эту папку".into(),
        callback_data: format!("p:{}:s", browser.nonce),
    }]];
    if browser.path.parent().is_some() {
        rows.push(vec![InlineKeyboardButton {
            text: "⬆️ Вверх".into(),
            callback_data: format!("p:{}:u", browser.nonce),
        }]);
    }
    for (index, path) in directories
        .iter()
        .enumerate()
        .skip(page * PROJECT_PAGE_SIZE)
        .take(PROJECT_PAGE_SIZE)
    {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .chars()
            .take(42)
            .collect::<String>();
        rows.push(vec![InlineKeyboardButton {
            text: format!("📁 {name}"),
            callback_data: format!("p:{}:d{index}", browser.nonce),
        }]);
    }
    let mut navigation = Vec::new();
    if page > 0 {
        navigation.push(InlineKeyboardButton {
            text: "◀️".into(),
            callback_data: format!("p:{}:b", browser.nonce),
        });
    }
    if page + 1 < total_pages {
        navigation.push(InlineKeyboardButton {
            text: "▶️".into(),
            callback_data: format!("p:{}:n", browser.nonce),
        });
    }
    if !navigation.is_empty() {
        rows.push(navigation);
    }
    let text = format!(
        "Текущая папка: {}\nВыбрана для заданий: {}\nСтраница {}/{}",
        browser.path.display(),
        selected.display(),
        page + 1,
        total_pages
    );
    Ok((
        text,
        InlineKeyboardMarkup {
            inline_keyboard: rows,
        },
    ))
}

async fn show_project_menu(
    client: &reqwest::Client,
    config: &Config,
    browser: &Browser,
    selected: &Path,
    message_id: i64,
    edit: bool,
) -> Result<(), BotError> {
    let (text, reply_markup) = project_menu(browser, selected)?;
    if edit {
        let _: serde_json::Value = telegram_post(
            client,
            &config.bot_token,
            "editMessageText",
            &EditMessageText {
                chat_id: config.allowed_user_id,
                message_id,
                text: &text,
                reply_markup,
            },
        )
        .await?;
    } else {
        let _: serde_json::Value = telegram_post(
            client,
            &config.bot_token,
            "sendMessage",
            &SendMessage {
                chat_id: config.allowed_user_id,
                text: &text,
                reply_to_message_id: message_id,
                reply_markup: Some(reply_markup),
            },
        )
        .await?;
    }
    Ok(())
}

async fn answer_callback(client: &reqwest::Client, config: &Config, id: &str, text: &str) {
    let result: Result<bool, _> = telegram_post(
        client,
        &config.bot_token,
        "answerCallbackQuery",
        &AnswerCallbackQuery {
            callback_query_id: id,
            text,
        },
    )
    .await;
    if let Err(error) = result {
        eprintln!("Ошибка ответа на нажатие кнопки: {error}");
    }
}

async fn send_document(
    client: &reqwest::Client,
    config: &Config,
    chat_id: i64,
    reply_to_message_id: i64,
    path: &Path,
    caption: &str,
) -> Result<(), BotError> {
    let data = tokio::fs::read(path).await?;
    let filename = path
        .file_name()
        .ok_or("у файла нет имени")?
        .to_string_lossy()
        .to_string();
    let part = reqwest::multipart::Part::bytes(data).file_name(filename);
    let form = reqwest::multipart::Form::new()
        .text("chat_id", chat_id.to_string())
        .text("reply_to_message_id", reply_to_message_id.to_string())
        .text("caption", caption.to_string())
        .part("document", part);
    let url = format!(
        "https://api.telegram.org/bot{}/sendDocument",
        config.bot_token
    );
    let response = client
        .post(url)
        .multipart(form)
        .send()
        .await
        .map_err(|error| format!("Ошибка загрузки файла в Telegram: {}", error.without_url()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("Telegram API вернул HTTP {status} при загрузке файла").into());
    }
    let payload: TelegramResponse<serde_json::Value> = response
        .json()
        .await
        .map_err(|error| format!("Ошибка ответа Telegram: {}", error.without_url()))?;
    if !payload.ok {
        return Err(payload
            .description
            .unwrap_or_else(|| "ошибка загрузки файла".into())
            .into());
    }
    Ok(())
}

async fn download_input(
    client: &reqwest::Client,
    config: &Config,
    file: &PendingFile,
    index: usize,
    input_dir: &Path,
) -> Result<PathBuf, BotError> {
    if file.size.is_some_and(|size| size > MAX_FILE_BYTES) {
        return Err(format!("Файл {} больше 20 МБ", file.name).into());
    }
    let info: TelegramFileInfo = telegram_post(
        client,
        &config.bot_token,
        "getFile",
        &GetFile {
            file_id: &file.file_id,
        },
    )
    .await?;
    if info.file_size.is_some_and(|size| size > MAX_FILE_BYTES) {
        return Err(format!("Файл {} больше 20 МБ", file.name).into());
    }
    let remote_path = info.file_path.ok_or("Telegram не вернул путь к файлу")?;
    let url = format!(
        "https://api.telegram.org/file/bot{}/{}",
        config.bot_token, remote_path
    );
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|error| format!("Ошибка скачивания из Telegram: {}", error.without_url()))?;
    if !response.status().is_success() {
        return Err(format!("Telegram вернул HTTP {} при скачивании", response.status()).into());
    }

    let extension = Path::new(&file.name)
        .extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| {
            extension.len() <= 10 && extension.chars().all(|ch| ch.is_ascii_alphanumeric())
        })
        .map(|extension| format!(".{extension}"))
        .unwrap_or_default();
    let path = input_dir.join(format!("input_{index}{extension}"));
    let mut output = tokio::fs::File::create(&path).await?;
    let mut bytes = 0u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("Ошибка скачивания из Telegram: {}", error.without_url()))?
    {
        bytes += chunk.len() as u64;
        if bytes > MAX_FILE_BYTES {
            return Err(format!("Файл {} больше 20 МБ", file.name).into());
        }
        output.write_all(&chunk).await?;
    }
    Ok(path)
}

fn collect_files(directory: &Path) -> std::io::Result<Vec<PathBuf>> {
    fn visit(directory: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let metadata = std::fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                visit(&entry.path(), files)?;
            } else if metadata.is_file() {
                files.push(entry.path());
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    visit(directory, &mut files)?;
    files.sort();
    Ok(files)
}

async fn send_task_files(
    client: &reqwest::Client,
    config: &Config,
    chat_id: i64,
    reply_to_message_id: i64,
    task_dir: &Path,
) -> Result<(), BotError> {
    let files = collect_files(task_dir)?;
    let canonical_task_dir = std::fs::canonicalize(task_dir)?;
    let mut sent = 0;
    let mut skipped = 0;
    for path in files {
        if !std::fs::canonicalize(&path)?.starts_with(&canonical_task_dir) {
            skipped += 1;
            continue;
        }
        let size = std::fs::metadata(&path)?.len();
        if sent >= MAX_FILES_PER_TASK || size > MAX_FILE_BYTES {
            skipped += 1;
            continue;
        }
        let caption = path
            .strip_prefix(task_dir)?
            .to_string_lossy()
            .chars()
            .take(1000)
            .collect::<String>();
        if let Err(error) = send_document(
            client,
            config,
            chat_id,
            reply_to_message_id,
            &path,
            &caption,
        )
        .await
        {
            eprintln!("Не удалось отправить файл {}: {error}", path.display());
            skipped += 1;
        } else {
            sent += 1;
        }
    }
    if skipped > 0 {
        let notice = format!(
            "Не удалось отправить {skipped} файл(ов). Они сохранены в {}",
            task_dir.display()
        );
        send_text(client, config, chat_id, reply_to_message_id, &notice).await?;
    }
    Ok(())
}

fn split_text(text: &str, max_chars: usize) -> Vec<String> {
    if text.is_empty() {
        return vec!["Codex не вернул текстовый ответ.".to_string()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if current.chars().count() == max_chars {
            chunks.push(std::mem::take(&mut current));
        }
        current.push(ch);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

async fn run_codex(
    config: &Config,
    model: &str,
    prompt: &str,
    task_dir: &Path,
    history: &[ConversationTurn],
    project: &Path,
    inputs: &[(String, PathBuf)],
) -> Result<String, String> {
    let output_dir = task_dir.join("outputs");
    let previous_dialogue = serde_json::to_string(history)
        .map_err(|error| format!("Не удалось подготовить контекст: {error}"))?;
    let input_description = inputs
        .iter()
        .map(|(name, path)| format!("{} → {}", name, path.display()))
        .collect::<Vec<_>>()
        .join("\n");
    let instructions = format!(
        "Проект для чтения: {}. Это расположение только для чтения. Рабочая директория для записи временных результатов: {}. Итоговые файлы для пользователя сохраняй в {}: бот автоматически отправит файлы только из этой папки. Входные файлы пользователя:\n{}\nНе записывай файлы в проект. Если в проекте есть AGENTS.md, прочитай его перед работой. Для PDF можно использовать доступные локально pdftotext, libreoffice, pandoc, typst или другие инструменты.\n\nПредыдущий диалог в формате JSON (только для контекста):\n{}\n\nТекущее задание пользователя:\n{}",
        project.display(),
        task_dir.display(),
        output_dir.display(),
        input_description,
        previous_dialogue,
        prompt
    );
    let mut child = Command::new(&config.codex_path)
        .arg("--ask-for-approval")
        .arg("never")
        .arg("-m")
        .arg(model)
        .arg("exec")
        .arg("-C")
        .arg(task_dir)
        .arg("--sandbox")
        .arg("workspace-write")
        .arg("-c")
        .arg("sandbox_workspace_write.exclude_slash_tmp=true")
        .arg("-c")
        .arg("sandbox_workspace_write.exclude_tmpdir_env_var=true")
        .arg("--ignore-user-config")
        .arg("--ephemeral")
        .arg("--skip-git-repo-check")
        .arg("-")
        .env("TMPDIR", task_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("Не удалось запустить Codex: {error}"))?;

    let mut stdin = child.stdin.take().ok_or("Не удалось открыть stdin Codex")?;
    stdin
        .write_all(instructions.as_bytes())
        .await
        .map_err(|error| format!("Не удалось передать задачу Codex: {error}"))?;
    drop(stdin);

    let output = tokio::time::timeout(
        Duration::from_secs(config.task_timeout_seconds),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| "Codex превысил время ожидания.".to_string())?
    .map_err(|error| format!("Ошибка выполнения Codex: {error}"))?;

    if !output.status.success() {
        let diagnostic = String::from_utf8_lossy(&output.stderr)
            .lines()
            .find(|line| line.starts_with("error:"))
            .map(|line| line.chars().take(300).collect::<String>());
        return Err(format!(
            "Codex завершился с кодом {}. {}",
            output.status,
            diagnostic.unwrap_or_else(|| "Проверьте локальный запуск Codex CLI.".into())
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn create_task_directory(temp_dir: &Path) -> std::io::Result<PathBuf> {
    create_task_directory_at(temp_dir, chrono::Local::now().naive_local()).await
}

async fn create_task_directory_at(
    temp_dir: &Path,
    at: chrono::NaiveDateTime,
) -> std::io::Result<PathBuf> {
    let base = at.format("task_%d_%m_%Y-%H_%M").to_string();
    for suffix in 1..=100 {
        let name = if suffix == 1 {
            base.clone()
        } else {
            format!("{base}_{suffix}")
        };
        let path = temp_dir.join(name);
        match tokio::fs::create_dir(&path).await {
            Ok(()) => {
                tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).await?;
                for name in ["inputs", "outputs"] {
                    let child = path.join(name);
                    tokio::fs::create_dir(&child).await?;
                    tokio::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o700))
                        .await?;
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "слишком много заданий за одну минуту",
    ))
}

async fn execute_task(
    client: reqwest::Client,
    config: Config,
    spec: TaskSpec,
    temp_dir: PathBuf,
) -> TaskOutcome {
    let mut outcome = TaskOutcome {
        prompt: spec.prompt.clone(),
        answer: None,
        context_generation: spec.context_generation,
    };
    let task_dir = match create_task_directory(&temp_dir).await {
        Ok(path) => path,
        Err(error) => {
            let message = format!("Не удалось создать папку задания: {error}");
            let _ = send_text(
                &client,
                &config,
                config.allowed_user_id,
                spec.reply_to_message_id,
                &message,
            )
            .await;
            return outcome;
        }
    };
    let mut inputs = Vec::new();
    for (index, file) in spec.files.iter().enumerate() {
        match download_input(&client, &config, file, index + 1, &task_dir.join("inputs")).await {
            Ok(path) => inputs.push((file.name.clone(), path)),
            Err(error) => {
                let message = format!("Не удалось получить файл {}: {error}", file.name);
                let _ = send_text(
                    &client,
                    &config,
                    config.allowed_user_id,
                    spec.reply_to_message_id,
                    &message,
                )
                .await;
                return outcome;
            }
        }
    }

    let result = run_codex(
        &config,
        &spec.model,
        &spec.prompt,
        &task_dir,
        &spec.history,
        &spec.project,
        &inputs,
    )
    .await;
    let response = result.as_deref().unwrap_or_else(|error| error);
    match send_text(
        &client,
        &config,
        config.allowed_user_id,
        spec.reply_to_message_id,
        response,
    )
    .await
    {
        Ok(()) if result.is_ok() => outcome.answer = Some(response.to_string()),
        Err(error) => eprintln!("Ошибка отправки ответа: {error}"),
        _ => {}
    }
    if let Err(error) = send_task_files(
        &client,
        &config,
        config.allowed_user_id,
        spec.reply_to_message_id,
        &task_dir.join("outputs"),
    )
    .await
    {
        eprintln!("Ошибка отправки подготовленных файлов: {error}");
    }
    outcome
}

async fn handle_message(
    client: &reqwest::Client,
    config: &Config,
    message: Message,
    update_id: i64,
    state: &mut State,
    state_path: &Path,
    active: &mut Option<ActiveTask>,
    browser: &mut Option<Browser>,
) -> Option<TaskSpec> {
    if !is_authorized(&message, config.allowed_user_id) {
        return None;
    }
    let prompt = message
        .text
        .as_deref()
        .or(message.caption.as_deref())
        .unwrap_or("")
        .trim();
    let reply = message.message_id;
    match prompt {
        "/start" | "/help" => {
            let help = "Отправьте задачу текстом или документ с подписью-заданием. Файл без подписи прикрепится к следующему заданию. /status — состояние, /cancel — остановить, /project — выбрать папку, /model — модель, /reset — контекст.";
            let _ = send_text(client, config, message.chat.id, reply, help).await;
            return None;
        }
        "/status" => {
            let activity = if let Some(task) = active.as_ref() {
                format!(
                    "Выполняется задание №{} ({} секунд). Модель: {}. Папка: {}.",
                    task.update_id,
                    task.started.elapsed().as_secs(),
                    task.model,
                    task.project.display()
                )
            } else {
                "Сейчас заданий нет.".to_string()
            };
            let status = format!(
                "{activity}\nВыбрана модель: {}\nТекущий проект: {}\nОжидают задания файлов: {}",
                state.model_slug(),
                state.project(config).display(),
                state.pending_files.len()
            );
            let _ = send_text(client, config, message.chat.id, reply, &status).await;
            return None;
        }
        "/cancel" => {
            let status = if let Some(task) = active.take() {
                task.handle.abort();
                "Задание остановлено. Подготовленные файлы остаются в temp."
            } else {
                "Сейчас нет задания для отмены."
            };
            let _ = send_text(client, config, message.chat.id, reply, status).await;
            return None;
        }
        "/model" => {
            if let Err(error) =
                send_model_menu(client, config, message.chat.id, reply, state.model_slug()).await
            {
                eprintln!("Ошибка отправки списка моделей: {error}");
            }
            return None;
        }
        "/project" => {
            let path = state.project(config).canonicalize();
            match path {
                Ok(path) => {
                    *browser = Some(Browser {
                        path,
                        page: 0,
                        nonce: update_id,
                    });
                    if let Some(browser) = browser.as_ref() {
                        if let Err(error) = show_project_menu(
                            client,
                            config,
                            browser,
                            state.project(config),
                            reply,
                            false,
                        )
                        .await
                        {
                            let _ = send_text(
                                client,
                                config,
                                message.chat.id,
                                reply,
                                &format!("Не удалось открыть папку: {error}"),
                            )
                            .await;
                        }
                    }
                }
                Err(error) => {
                    let _ = send_text(
                        client,
                        config,
                        message.chat.id,
                        reply,
                        &format!("Не удалось открыть папку: {error}"),
                    )
                    .await;
                }
            }
            return None;
        }
        "/reset" => {
            let previous = state.clone();
            state.history.clear();
            state.pending_files.clear();
            state.context_generation += 1;
            let status = if let Err(error) = save_state(state_path, state).await {
                *state = previous;
                eprintln!("Не удалось сбросить контекст: {error}");
                "Не удалось сбросить контекст."
            } else {
                if let Some(task) = active.take() {
                    task.handle.abort();
                }
                "Контекст и ожидающие файлы сброшены. Текущее задание остановлено. Модель и проект сохранены."
            };
            let _ = send_text(client, config, message.chat.id, reply, status).await;
            return None;
        }
        _ => {}
    }

    let attachment = if let Some(document) = message.document.as_ref() {
        Some(PendingFile {
            file_id: document.file_id.clone(),
            name: document
                .file_name
                .clone()
                .unwrap_or_else(|| "document".into()),
            size: document.file_size,
        })
    } else {
        message
            .photo
            .as_ref()
            .and_then(|photos| photos.last())
            .map(|photo| PendingFile {
                file_id: photo.file_id.clone(),
                name: "photo.jpg".into(),
                size: photo.file_size,
            })
    };
    if let Some(file) = attachment {
        if file.size.is_some_and(|size| size > MAX_FILE_BYTES) {
            let _ = send_text(
                client,
                config,
                message.chat.id,
                reply,
                "Файл больше 20 МБ — Telegram Bot API не позволит его скачать.",
            )
            .await;
            return None;
        }
        if state.pending_files.len() >= MAX_INPUT_FILES {
            let _ = send_text(
                client,
                config,
                message.chat.id,
                reply,
                "Достигнут предел: 10 файлов на задание.",
            )
            .await;
            return None;
        }
        state.pending_files.push(file);
        if prompt.is_empty() || active.is_some() {
            let response = if active.is_some() && !prompt.is_empty() {
                "Файл добавлен. Сейчас выполняется другое задание; повторите текст задания после его завершения."
            } else {
                "Файл добавлен. Пришлите текст задания следующим сообщением."
            };
            let _ = send_text(client, config, message.chat.id, reply, response).await;
            return None;
        }
    }
    if prompt.is_empty() {
        return None;
    }
    if active.is_some() {
        let _ = send_text(client, config, message.chat.id, reply, "Другое задание ещё выполняется. Используйте /status или /cancel, затем повторите запрос.").await;
        return None;
    }

    let files = std::mem::take(&mut state.pending_files);
    let spec = TaskSpec {
        update_id,
        reply_to_message_id: reply,
        prompt: prompt.to_string(),
        files,
        model: state.model_slug().to_string(),
        project: state.project(config).to_path_buf(),
        history: state.history.clone(),
        context_generation: state.context_generation,
    };
    let _ = send_text(
        client,
        config,
        message.chat.id,
        reply,
        "Задача принята. Выполняю…",
    )
    .await;
    Some(spec)
}

async fn handle_model_callback(
    client: &reqwest::Client,
    config: &Config,
    query: CallbackQuery,
    state: &mut State,
    state_path: &Path,
) {
    if !is_authorized_callback(&query, config.allowed_user_id) {
        return;
    }
    let Some(message) = query.message else {
        return;
    };
    let Some(slug) = query
        .data
        .as_deref()
        .and_then(|data| data.strip_prefix("model:"))
    else {
        return;
    };
    let Some(name) = model_name(slug) else {
        return;
    };

    let previous = std::mem::replace(&mut state.selected_model, slug.to_string());
    let response = if let Err(error) = save_state(state_path, state).await {
        state.selected_model = previous;
        eprintln!("Не удалось сохранить выбранную модель: {error}");
        "Не удалось сохранить модель. Попробуйте ещё раз.".to_string()
    } else {
        format!("Выбрана модель {name}. Она будет использоваться для следующих заданий.")
    };

    let answer: Result<bool, _> = telegram_post(
        client,
        &config.bot_token,
        "answerCallbackQuery",
        &AnswerCallbackQuery {
            callback_query_id: &query.id,
            text: &response,
        },
    )
    .await;
    if let Err(error) = answer {
        eprintln!("Ошибка ответа на нажатие кнопки: {error}");
    }
    if let Err(error) = send_text(
        client,
        config,
        message.chat.id,
        message.message_id,
        &response,
    )
    .await
    {
        eprintln!("Ошибка отправки выбранной модели: {error}");
    }
}

async fn handle_project_callback(
    client: &reqwest::Client,
    config: &Config,
    query: CallbackQuery,
    update_id: i64,
    state: &mut State,
    state_path: &Path,
    browser: &mut Option<Browser>,
) {
    if !is_authorized_callback(&query, config.allowed_user_id) {
        return;
    }
    let Some(message) = query.message else {
        return;
    };
    let Some(data) = query.data.as_deref() else {
        return;
    };
    let mut parts = data.split(':');
    if parts.next() != Some("p") {
        return;
    }
    let nonce = parts.next().and_then(|value| value.parse::<i64>().ok());
    let action = parts.next();
    if parts.next().is_some()
        || browser
            .as_ref()
            .is_none_or(|current| Some(current.nonce) != nonce)
    {
        answer_callback(
            client,
            config,
            &query.id,
            "Меню устарело. Откройте /project снова.",
        )
        .await;
        return;
    }
    let Some(action) = action else {
        return;
    };
    if action == "s" {
        let path = match browser.as_ref().unwrap().path.canonicalize() {
            Ok(path) if path.is_dir() => path,
            _ => {
                answer_callback(client, config, &query.id, "Папка недоступна.").await;
                return;
            }
        };
        let previous = state.clone();
        state.selected_project = Some(path.clone());
        state.history.clear();
        state.context_generation += 1;
        if let Err(error) = save_state(state_path, state).await {
            *state = previous;
            eprintln!("Не удалось сохранить проект: {error}");
            answer_callback(client, config, &query.id, "Не удалось сохранить проект.").await;
            return;
        }
        *browser = None;
        answer_callback(client, config, &query.id, "Папка выбрана.").await;
        let response = format!(
            "Выбран проект: {}\nКонтекст диалога сброшен.",
            path.display()
        );
        let _ = send_text(
            client,
            config,
            message.chat.id,
            message.message_id,
            &response,
        )
        .await;
        return;
    }

    let current = browser.as_mut().unwrap();
    let old_path = current.path.clone();
    let old_page = current.page;
    let old_nonce = current.nonce;
    let navigation = match action {
        "u" => current.path.parent().map(Path::to_path_buf),
        "b" => {
            current.page = current.page.saturating_sub(1);
            Some(current.path.clone())
        }
        "n" => {
            current.page += 1;
            Some(current.path.clone())
        }
        value if value.starts_with('d') => {
            let index = value[1..].parse::<usize>().ok();
            subdirectories(&current.path)
                .ok()
                .and_then(|directories| index.and_then(|index| directories.get(index).cloned()))
        }
        _ => None,
    };
    let Some(path) = navigation else {
        answer_callback(client, config, &query.id, "Папка недоступна.").await;
        return;
    };
    current.path = path;
    if action == "u" || action.starts_with('d') {
        current.page = 0;
    }
    current.nonce = update_id;
    if let Err(error) = show_project_menu(
        client,
        config,
        current,
        state.project(config),
        message.message_id,
        true,
    )
    .await
    {
        current.path = old_path;
        current.page = old_page;
        current.nonce = old_nonce;
        eprintln!("Ошибка навигации по папкам: {error}");
        answer_callback(client, config, &query.id, "Не удалось открыть папку.").await;
    } else {
        answer_callback(client, config, &query.id, "").await;
    }
}

fn load_state(path: &Path) -> State {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

async fn save_state(path: &Path, state: &State) -> Result<(), Box<dyn std::error::Error>> {
    let temp_path = path.with_extension("json.tmp");
    tokio::fs::write(&temp_path, serde_json::to_vec(state)?).await?;
    tokio::fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(0o600)).await?;
    tokio::fs::rename(temp_path, path).await?;
    Ok(())
}

enum Event {
    Updates(Result<Vec<Update>, BotError>),
    TaskDone(Result<TaskOutcome, tokio::task::JoinError>),
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".into());
    let config = Config::load(Path::new(&config_path))?;
    let temp_dir = std::env::current_dir()?.join("temp");
    tokio::fs::create_dir_all(&temp_dir).await?;
    tokio::fs::set_permissions(&temp_dir, std::fs::Permissions::from_mode(0o700)).await?;
    let state_path = Path::new("state.json");
    let mut state = load_state(state_path);
    if state_path.exists() {
        tokio::fs::set_permissions(state_path, std::fs::Permissions::from_mode(0o600)).await?;
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(45))
        .build()?;
    let mut active: Option<ActiveTask> = None;
    let mut browser: Option<Browser> = None;

    let commands = SetCommands {
        commands: vec![
            BotCommand {
                command: "status",
                description: "Состояние задания",
            },
            BotCommand {
                command: "cancel",
                description: "Остановить задание",
            },
            BotCommand {
                command: "project",
                description: "Выбрать папку",
            },
            BotCommand {
                command: "model",
                description: "Выбрать модель",
            },
            BotCommand {
                command: "reset",
                description: "Очистить контекст",
            },
            BotCommand {
                command: "help",
                description: "Как пользоваться ботом",
            },
        ],
        scope: BotCommandScope {
            kind: "chat",
            chat_id: config.allowed_user_id,
        },
    };
    let registered: Result<bool, _> =
        telegram_post(&client, &config.bot_token, "setMyCommands", &commands).await;
    if let Err(error) = registered {
        eprintln!("Не удалось зарегистрировать команды Telegram: {error}");
    }

    println!(
        "Бот запущен. Разрешённый Telegram user ID: {}",
        config.allowed_user_id
    );
    loop {
        let request = GetUpdates {
            offset: state.next_offset,
            timeout: 30,
            allowed_updates: ["message", "callback_query"],
        };
        let updates = telegram_post(&client, &config.bot_token, "getUpdates", &request);
        let event = if let Some(task) = active.as_mut() {
            tokio::select! {
                biased;
                result = &mut task.handle => Event::TaskDone(result),
                result = updates => Event::Updates(result),
            }
        } else {
            Event::Updates(updates.await)
        };

        match event {
            Event::TaskDone(result) => {
                let task = active.take().unwrap();
                match result {
                    Ok(outcome) => {
                        if state.context_generation == outcome.context_generation {
                            if let Some(answer) = outcome.answer {
                                state.remember(&outcome.prompt, &answer);
                                if let Err(error) = save_state(state_path, &state).await {
                                    eprintln!("Не удалось сохранить контекст: {error}");
                                }
                            }
                        }
                    }
                    Err(error) => {
                        eprintln!("Ошибка задачи: {error}");
                        let _ = send_text(
                            &client,
                            &config,
                            config.allowed_user_id,
                            task.reply_to_message_id,
                            "Задание завершилось с внутренней ошибкой.",
                        )
                        .await;
                    }
                }
            }
            Event::Updates(Ok(updates)) => {
                for update in updates {
                    if update.update_id < state.next_offset {
                        continue;
                    }
                    if let Some(message) = update.message {
                        if let Some(spec) = handle_message(
                            &client,
                            &config,
                            message,
                            update.update_id,
                            &mut state,
                            state_path,
                            &mut active,
                            &mut browser,
                        )
                        .await
                        {
                            let model = spec.model.clone();
                            let project = spec.project.clone();
                            let reply_to_message_id = spec.reply_to_message_id;
                            let update_id = spec.update_id;
                            let handle = tokio::spawn(execute_task(
                                client.clone(),
                                config.clone(),
                                spec,
                                temp_dir.clone(),
                            ));
                            active = Some(ActiveTask {
                                handle,
                                started: Instant::now(),
                                update_id,
                                model,
                                project,
                                reply_to_message_id,
                            });
                        }
                    }
                    if let Some(query) = update.callback_query {
                        if query
                            .data
                            .as_deref()
                            .is_some_and(|data| data.starts_with("p:"))
                        {
                            handle_project_callback(
                                &client,
                                &config,
                                query,
                                update.update_id,
                                &mut state,
                                state_path,
                                &mut browser,
                            )
                            .await;
                        } else {
                            handle_model_callback(&client, &config, query, &mut state, state_path)
                                .await;
                        }
                    }
                    state.next_offset = update.update_id + 1;
                    if let Err(error) = save_state(state_path, &state).await {
                        eprintln!("Не удалось сохранить позицию Telegram: {error}");
                    }
                }
            }
            Event::Updates(Err(error)) => {
                eprintln!("Ошибка Telegram API: {error}");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_owner_in_private_chat_is_authorized() {
        let mut message = Message {
            message_id: 1,
            chat: Chat {
                id: 42,
                kind: "private".into(),
            },
            from: Some(User { id: 42 }),
            text: Some("test".into()),
            caption: None,
            document: None,
            photo: None,
        };
        assert!(is_authorized(&message, 42));
        message.from = Some(User { id: 43 });
        assert!(!is_authorized(&message, 42));
        message.from = Some(User { id: 42 });
        message.chat.kind = "group".into();
        assert!(!is_authorized(&message, 42));
    }

    #[test]
    fn splits_unicode_without_losing_characters() {
        let text = "Оценки: 5️⃣ 4️⃣ 3️⃣";
        let chunks = split_text(text, 5);
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 5));
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn excludes_symlinks_from_prepared_files() {
        let root = std::env::temp_dir().join(format!("myarchmonkey_test_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("report.txt"), "report").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", root.join("outside.txt")).unwrap();
        let files = collect_files(&root).unwrap();
        assert_eq!(files, vec![root.join("report.txt")]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn model_selection_is_limited_to_owner_in_private_chat() {
        let mut query = CallbackQuery {
            id: "test".into(),
            from: User { id: 42 },
            message: Some(Message {
                message_id: 1,
                chat: Chat {
                    id: 42,
                    kind: "private".into(),
                },
                from: None,
                text: None,
                caption: None,
                document: None,
                photo: None,
            }),
            data: Some("model:gpt-6-luna".into()),
        };
        assert!(is_authorized_callback(&query, 42));
        query.from.id = 43;
        assert!(!is_authorized_callback(&query, 42));
        query.from.id = 42;
        query.message.as_mut().unwrap().chat.kind = "group".into();
        assert!(!is_authorized_callback(&query, 42));
        assert!(model_name("gpt-6-luna").is_some());
        assert!(model_name("unknown").is_none());
    }

    #[test]
    fn old_state_loads_default_model() {
        let state: State = serde_json::from_str(r#"{"next_offset":123}"#).unwrap();
        assert_eq!(state.model_slug(), DEFAULT_MODEL);
        assert!(state.history.is_empty());
        assert!(state.selected_project.is_none());
        assert!(state.pending_files.is_empty());
    }

    #[test]
    fn conversation_history_is_bounded_and_survives_serialization() {
        let mut state = State::default();
        state.selected_model = "gpt-6-luna".into();
        for number in 0..=MAX_HISTORY_TURNS {
            state.remember(&format!("вопрос {number}"), "ответ");
        }
        assert_eq!(state.history.len(), MAX_HISTORY_TURNS);
        assert_eq!(state.history[0].user, "вопрос 1");

        let restored: State = serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(restored.model_slug(), "gpt-6-luna");
        assert_eq!(restored.history.len(), MAX_HISTORY_TURNS);
    }

    #[tokio::test]
    async fn task_directories_use_local_time_and_unique_suffix() {
        let root = std::env::temp_dir().join(format!("myarchmonkey_dirs_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let at = chrono::NaiveDate::from_ymd_opt(2026, 11, 22)
            .unwrap()
            .and_hms_opt(10, 30, 0)
            .unwrap();
        let first = create_task_directory_at(&root, at).await.unwrap();
        let second = create_task_directory_at(&root, at).await.unwrap();
        assert_eq!(
            first.file_name().unwrap().to_string_lossy(),
            "task_22_11_2026-10_30"
        );
        assert_eq!(
            second.file_name().unwrap().to_string_lossy(),
            "task_22_11_2026-10_30_2"
        );
        assert!(first.join("inputs").is_dir());
        assert!(first.join("outputs").is_dir());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn telegram_document_with_caption_is_readable() {
        let message: Message = serde_json::from_str(
            r#"{"message_id":7,"chat":{"id":42,"type":"private"},"from":{"id":42},"caption":"сделай новый PDF","document":{"file_id":"abc","file_name":"source.pdf","file_size":1000}}"#,
        )
        .unwrap();
        assert_eq!(message.caption.as_deref(), Some("сделай новый PDF"));
        assert_eq!(
            message.document.unwrap().file_name.as_deref(),
            Some("source.pdf")
        );
    }

    #[test]
    fn project_menu_lists_subdirectories_and_can_select_current() {
        let root =
            std::env::temp_dir().join(format!("myarchmonkey_project_{}", std::process::id()));
        std::fs::create_dir_all(root.join("folder_a")).unwrap();
        std::fs::create_dir_all(root.join("folder_b")).unwrap();
        std::fs::write(root.join("file.txt"), "ignored").unwrap();
        let browser = Browser {
            path: root.clone(),
            page: 0,
            nonce: 99,
        };
        let (text, markup) = project_menu(&browser, &root).unwrap();
        assert!(text.contains(&root.display().to_string()));
        let callbacks = markup
            .inline_keyboard
            .iter()
            .flatten()
            .map(|button| button.callback_data.as_str())
            .collect::<Vec<_>>();
        assert!(callbacks.contains(&"p:99:s"));
        assert!(callbacks.contains(&"p:99:d0"));
        assert!(callbacks.contains(&"p:99:d1"));
        assert!(!callbacks.contains(&"p:99:d2"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
