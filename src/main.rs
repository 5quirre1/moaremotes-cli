use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute, queue,
    style::Print,
    terminal::{self, ClearType},
};
use futures::future::join_all;
use reqwest;
use serde_json::{from_str, Value};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, stdout, Write},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio;

const DEFAULT_URL: &str = "https://raw.githubusercontent.com/JustAGoodUsername/moaremotes-ext/refs/heads/patch-2/emotes.json";
const MAX_CONCURRENT_CHECKS: usize = 50;
const REQUEST_TIMEOUT: u64 = 3;

#[derive(Debug, Clone)]
struct EditorState {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
}

#[derive(Debug)]
struct Moaremotes {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
    scroll_offset: usize,
    terminal_height: usize,
    terminal_width: usize,
    duplicates: HashSet<usize>,
    missing_emotes: HashSet<usize>,
    status_message: String,
    filename: String,
    modified: bool,
    last_screen_hash: u64,
    checking_urls: bool,
    check_progress: (usize, usize),
    undo_stack: Vec<EditorState>,
    redo_stack: Vec<EditorState>,
    max_undo_size: usize,
}

impl Moaremotes {
    fn new() -> io::Result<Self> {
        let (width, height) = terminal::size()?;
        Ok(Moaremotes {
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
            scroll_offset: 0,
            terminal_height: height as usize,
            terminal_width: width as usize,
            duplicates: HashSet::new(),
            missing_emotes: HashSet::new(),
            status_message: "welcome to moaremotes emote thing!! press F1 for help".to_string(),
            filename: "emotes.json".to_string(),
            modified: false,
            last_screen_hash: 0,
            checking_urls: false,
            check_progress: (0, 0),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            max_undo_size: 100,
        })
    }

    fn load_from_content(&mut self, content: &str) -> Result<(), String> {
        match from_str::<Value>(content) {
            Ok(_) => {
                self.lines = content.lines().map(|line| line.to_string()).collect();
                if self.lines.is_empty() {
                    self.lines.push(String::new());
                }
                self.cursor_row = 0;
                self.cursor_col = 0;
                self.scroll_offset = 0;
                self.modified = false;
                self.check_duplicates();
                Ok(())
            }
            Err(e) => Err(format!("invalid json: {}", e)),
        }
    }

    async fn load_from_url(&mut self, url: &str) -> Result<(), String> {
        self.status_message = format!("loading from {}...", url);
        self.force_refresh_screen()
            .map_err(|e| format!("display error: {}", e))?;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| format!("failed to create client: {}", e))?;

        match client.get(url).send().await {
            Ok(response) => {
                if response.status().is_success() {
                    match response.text().await {
                        Ok(content) => {
                            self.load_from_content(&content)?;
                            fs::write(&self.filename, &content)
                                .map_err(|e| format!("failed to save file: {}", e))?;
                            self.status_message = format!("successfully loaded from {}", url);
                            Ok(())
                        }
                        Err(e) => Err(format!("failed to read response: {}", e)),
                    }
                } else {
                    Err(format!("HTTP error: {}", response.status()))
                }
            }
            Err(e) => Err(format!("request failed: {}", e)),
        }
    }

    fn load_from_file(&mut self, path: &str) -> Result<(), String> {
        match fs::read_to_string(path) {
            Ok(content) => {
                self.load_from_content(&content)?;
                self.filename = path.to_string();
                self.status_message = format!("loaded from {}", path);
                Ok(())
            }
            Err(e) => Err(format!("failed to read file {}: {}", path, e)),
        }
    }

    fn save_file(&mut self) -> Result<(), String> {
        let content = self.lines.join("\n");

        match from_str::<Value>(&content) {
            Ok(_) => {
                fs::write(&self.filename, &content)
                    .map_err(|e| format!("failed to save: {}", e))?;
                self.modified = false;
                self.status_message = format!("saved to {}", self.filename);
                Ok(())
            }
            Err(e) => Err(format!("cannot save invalid JSON: {}", e)),
        }
    }

    fn extract_url_from_line(&self, line: &str) -> Option<String> {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && (trimmed.ends_with("],") || trimmed.ends_with("]")) {
            if let Some(start) = trimmed.find('"') {
                if let Some(end) = trimmed[start + 1..].find('"') {
                    let url = &trimmed[start + 1..start + 1 + end];
                    if url.starts_with("http") {
                        return Some(url.to_string());
                    }
                }
            }
        }
        None
    }

    fn save_state(&mut self) {
        let state = EditorState {
            lines: self.lines.clone(),
            cursor_row: self.cursor_row,
            cursor_col: self.cursor_col,
        };

        self.undo_stack.push(state);
        if self.undo_stack.len() > self.max_undo_size {
            self.undo_stack.remove(0);
        }

        self.redo_stack.clear();
    }

    fn undo(&mut self) {
        if let Some(state) = self.undo_stack.pop() {
            let current_state = EditorState {
                lines: self.lines.clone(),
                cursor_row: self.cursor_row,
                cursor_col: self.cursor_col,
            };
            self.redo_stack.push(current_state);

            self.lines = state.lines;
            self.cursor_row = state.cursor_row;
            self.cursor_col = state.cursor_col;

            self.modified = true;
            self.check_duplicates();
            self.status_message = "undo".to_string();
        } else {
            self.status_message = "nothing to undo".to_string();
        }
    }

    fn redo(&mut self) {
        if let Some(state) = self.redo_stack.pop() {
            let current_state = EditorState {
                lines: self.lines.clone(),
                cursor_row: self.cursor_row,
                cursor_col: self.cursor_col,
            };
            self.undo_stack.push(current_state);

            self.lines = state.lines;
            self.cursor_row = state.cursor_row;
            self.cursor_col = state.cursor_col;

            self.modified = true;
            self.check_duplicates();
            self.status_message = "redo".to_string();
        } else {
            self.status_message = "nothing to redo".to_string();
        }
    }

    fn check_duplicates(&mut self) {
        self.duplicates.clear();
        let mut seen_urls = HashMap::new();

        for (i, line) in self.lines.iter().enumerate() {
            if let Some(url) = self.extract_url_from_line(line) {
                if let Some(&first_occurrence) = seen_urls.get(&url) {
                    self.duplicates.insert(i);
                    self.duplicates.insert(first_occurrence);
                } else {
                    seen_urls.insert(url, i);
                }
            }
        }
    }

    async fn check_missing_emotes(&mut self) -> Result<(), String> {
        self.checking_urls = true;
        self.missing_emotes.clear();

        let client = Arc::new(reqwest::Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36")
        .pool_max_idle_per_host(20)
        .pool_idle_timeout(Duration::from_secs(30))
        .danger_accept_invalid_certs(false)
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()
        .map_err(|e| format!("failed to create client: {}", e))?);

        let mut urls_to_check = Vec::new();
        for (i, line) in self.lines.iter().enumerate() {
            if let Some(url) = self.extract_url_from_line(line) {
                urls_to_check.push((i, url));
            }
        }

        let total = urls_to_check.len();
        self.check_progress = (0, total);

        if total == 0 {
            self.checking_urls = false;
            self.status_message = "no urls found to check".to_string();
            return Ok(());
        }

        let start_time = Instant::now();
        let missing_emotes = Arc::new(Mutex::new(HashSet::new()));
        let progress_counter = Arc::new(Mutex::new(0usize));
        let good_urls = Arc::new(Mutex::new(0usize));

        let test_url = &urls_to_check[0].1;
        self.status_message = format!("testing connection to {}...", test_url);
        let _ = self.refresh_screen();

        match client.head(test_url).send().await {
            Ok(response) => {
                self.status_message = format!(
                    "test response: {} - {}",
                    response.status(),
                    response.status().canonical_reason().unwrap_or("Unknown")
                );
                let _ = self.refresh_screen();
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
            Err(e) => {
                self.status_message = format!("test failed: {}", e);
                let _ = self.refresh_screen();
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
        }

        let chunk_size = std::cmp::min(MAX_CONCURRENT_CHECKS, 20);

        for chunk in urls_to_check.chunks(chunk_size) {
            let chunk_futures: Vec<_> = chunk
                .iter()
                .map(|(line_idx, url)| {
                    let client = client.clone();
                    let url = url.clone();
                    let line_idx = *line_idx;
                    let missing_emotes = missing_emotes.clone();
                    let progress_counter = progress_counter.clone();
                    let good_urls = good_urls.clone();

                    async move {
                        let mut is_missing = false;
                        let mut status_info = String::new();

                        match client.head(&url).send().await {
                            Ok(response) => {
                                let status = response.status();
                                status_info = format!("{}", status.as_u16());

                                if status.is_success() {
                                    if let Ok(mut good) = good_urls.lock() {
                                        *good += 1;
                                    }
                                } else if status.as_u16() == 405 {
                                    match client.get(&url).send().await {
                                        Ok(get_response) => {
                                            let get_status = get_response.status();
                                            status_info = format!(
                                                "HEAD:{}, GET:{}",
                                                status.as_u16(),
                                                get_status.as_u16()
                                            );
                                            if !get_status.is_success() {
                                                is_missing = true;
                                            } else {
                                                if let Ok(mut good) = good_urls.lock() {
                                                    *good += 1;
                                                }
                                            }
                                        }
                                        Err(_) => {
                                            is_missing = true;
                                            status_info =
                                                format!("HEAD:{}, GET:FAIL", status.as_u16());
                                        }
                                    }
                                } else {
                                    is_missing = true;
                                }
                            }
                            Err(e) => {
                                is_missing = true;
                                status_info = format!(
                                    "ERROR: {}",
                                    e.to_string().chars().take(20).collect::<String>()
                                );
                            }
                        };

                        if is_missing {
                            if let Ok(mut missing) = missing_emotes.lock() {
                                missing.insert(line_idx);
                            }
                        }

                        if let Ok(mut counter) = progress_counter.lock() {
                            *counter += 1;
                        }

                        if line_idx < 5 {
                            println!("debug: URL {} -> {}", url, status_info);
                        }
                    }
                })
                .collect();

            join_all(chunk_futures).await;

            if let Ok(completed) = progress_counter.lock() {
                self.check_progress = (*completed, total);
                let missing_count = if let Ok(missing) = missing_emotes.lock() {
                    missing.len()
                } else {
                    0
                };
                let good_count = if let Ok(good) = good_urls.lock() {
                    *good
                } else {
                    0
                };

                if let Ok(missing) = missing_emotes.lock() {
                    self.missing_emotes = missing.clone();
                }
                self.status_message = format!(
                    "checking urls... {}/{} ({} good, {} missing, {:.1}s)",
                    *completed,
                    total,
                    good_count,
                    missing_count,
                    start_time.elapsed().as_secs_f32()
                );

                let _ = self.refresh_screen();
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        if let Ok(missing) = missing_emotes.lock() {
            self.missing_emotes = missing.clone();
        }

        let elapsed = start_time.elapsed();
        let missing_count = self.missing_emotes.len();
        let good_count = if let Ok(good) = good_urls.lock() {
            *good
        } else {
            0
        };

        self.checking_urls = false;
        self.status_message = if missing_count == 0 {
            format!(
                "all {} emotes are real! ({:.1}s)",
                total,
                elapsed.as_secs_f32()
            )
        } else {
            format!("found {} good, {} missing/404 emotes in {:.1}s. press f5 to remove missing ones.", good_count, missing_count, elapsed.as_secs_f32())
        };

        Ok(())
    }

    fn remove_missing_emotes(&mut self) {
        let mut to_remove: Vec<usize> = self.missing_emotes.iter().cloned().collect();
        to_remove.sort_by(|a, b| b.cmp(a));

        let count = to_remove.len();
        for &i in &to_remove {
            if i < self.lines.len() {
                self.lines.remove(i);
                if self.cursor_row >= i && self.cursor_row > 0 {
                    self.cursor_row -= 1;
                }
            }
        }

        if count > 0 {
            self.modified = true;
            self.missing_emotes.clear();
            self.check_duplicates();
            self.status_message = format!("removed {} 404 emote(s)", count);
        }
    }

    fn remove_duplicates(&mut self) {
        let mut seen_urls = HashSet::new();
        let mut to_remove = Vec::new();

        for (i, line) in self.lines.iter().enumerate() {
            if let Some(url) = self.extract_url_from_line(line) {
                if !seen_urls.insert(url) {
                    to_remove.push(i);
                }
            }
        }

        for &i in to_remove.iter().rev() {
            self.lines.remove(i);
            if self.cursor_row >= i && self.cursor_row > 0 {
                self.cursor_row -= 1;
            }
        }

        self.modified = true;
        self.check_duplicates();
        self.status_message = format!("removed {} duplicate(s)", to_remove.len());
    }

    fn insert_char(&mut self, c: char) {
        if self.cursor_row >= self.lines.len() {
            return;
        }

        self.save_state();
        let line = &mut self.lines[self.cursor_row];
        if self.cursor_col <= line.len() {
            line.insert(self.cursor_col, c);
            self.cursor_col += 1;
            self.modified = true;
            self.check_duplicates();
        }
    }

    fn delete_char(&mut self) {
        if self.cursor_row >= self.lines.len() {
            return;
        }

        self.save_state();
        let line = &mut self.lines[self.cursor_row];
        if self.cursor_col > 0 && self.cursor_col <= line.len() {
            line.remove(self.cursor_col - 1);
            self.cursor_col -= 1;
            self.modified = true;
            self.check_duplicates();
        } else if self.cursor_col == 0 && self.cursor_row > 0 {
            let current_line = self.lines.remove(self.cursor_row);
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].len();
            self.lines[self.cursor_row].push_str(&current_line);
            self.modified = true;
            self.check_duplicates();
        }
    }

    fn insert_newline(&mut self) {
        if self.cursor_row >= self.lines.len() {
            return;
        }

        self.save_state();
        let line = &self.lines[self.cursor_row];
        let new_line = line[self.cursor_col..].to_string();
        self.lines[self.cursor_row] = line[..self.cursor_col].to_string();
        self.lines.insert(self.cursor_row + 1, new_line);
        self.cursor_row += 1;
        self.cursor_col = 0;
        self.modified = true;
        self.check_duplicates();
    }

    fn move_cursor(&mut self, direction: KeyCode) {
        match direction {
            KeyCode::Up => {
                if self.cursor_row > 0 {
                    self.cursor_row -= 1;
                    let line_len = self.lines.get(self.cursor_row).map_or(0, |l| l.len());
                    if self.cursor_col > line_len {
                        self.cursor_col = line_len;
                    }
                }
            }
            KeyCode::Down => {
                if self.cursor_row < self.lines.len() - 1 {
                    self.cursor_row += 1;
                    let line_len = self.lines.get(self.cursor_row).map_or(0, |l| l.len());
                    if self.cursor_col > line_len {
                        self.cursor_col = line_len;
                    }
                }
            }
            KeyCode::Left => {
                if self.cursor_col > 0 {
                    self.cursor_col -= 1;
                } else if self.cursor_row > 0 {
                    self.cursor_row -= 1;
                    self.cursor_col = self.lines.get(self.cursor_row).map_or(0, |l| l.len());
                }
            }
            KeyCode::Right => {
                let line_len = self.lines.get(self.cursor_row).map_or(0, |l| l.len());
                if self.cursor_col < line_len {
                    self.cursor_col += 1;
                } else if self.cursor_row < self.lines.len() - 1 {
                    self.cursor_row += 1;
                    self.cursor_col = 0;
                }
            }
            _ => {}
        }
    }

    fn adjust_scroll(&mut self) {
        let visible_height = self.terminal_height.saturating_sub(3);

        if self.cursor_row < self.scroll_offset {
            self.scroll_offset = self.cursor_row;
        } else if self.cursor_row >= self.scroll_offset + visible_height {
            self.scroll_offset = self.cursor_row - visible_height + 1;
        }
    }

    fn hash_screen_state(&self) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        self.cursor_row.hash(&mut hasher);
        self.cursor_col.hash(&mut hasher);
        self.scroll_offset.hash(&mut hasher);
        self.duplicates.len().hash(&mut hasher);
        self.missing_emotes.len().hash(&mut hasher);
        self.status_message.hash(&mut hasher);
        self.modified.hash(&mut hasher);
        self.checking_urls.hash(&mut hasher);
        self.check_progress.hash(&mut hasher);

        let visible_height = self.terminal_height.saturating_sub(3);
        for i in
            self.scroll_offset..std::cmp::min(self.scroll_offset + visible_height, self.lines.len())
        {
            self.lines[i].hash(&mut hasher);
            self.duplicates.contains(&i).hash(&mut hasher);
            self.missing_emotes.contains(&i).hash(&mut hasher);
        }

        hasher.finish()
    }

    fn refresh_screen(&mut self) -> io::Result<()> {
        let current_hash = self.hash_screen_state();

        if current_hash == self.last_screen_hash && !self.checking_urls {
            return Ok(());
        }

        self.force_refresh_screen()?;
        self.last_screen_hash = current_hash;
        Ok(())
    }

    fn force_refresh_screen(&mut self) -> io::Result<()> {
        let mut stdout = stdout();

        self.adjust_scroll();
        let visible_height = self.terminal_height.saturating_sub(3);

        let mut output = String::with_capacity(8192);

        output.push_str("\x1b[2J\x1b[H");

        for i in 0..visible_height {
            let line_idx = self.scroll_offset + i;

            if line_idx < self.lines.len() {
                let line = &self.lines[line_idx];
                let is_duplicate = self.duplicates.contains(&line_idx);
                let is_missing = self.missing_emotes.contains(&line_idx);

                output.push_str(&format!("\x1b[90m{:4} \x1b[0m", line_idx + 1));

                let display_line = if line.len() > self.terminal_width.saturating_sub(25) {
                    &line[..self.terminal_width.saturating_sub(25)]
                } else {
                    line
                };

                if is_duplicate {
                    output.push_str("\x1b[41m\x1b[37m");
                    output.push_str(display_line);
                    output.push_str("\x1b[0m ← DUPLICATE");
                } else if is_missing {
                    output.push_str("\x1b[45m\x1b[37m");
                    output.push_str(display_line);
                    output.push_str("\x1b[0m ← MISSING/404");
                } else {
                    output.push_str(display_line);
                }
            } else {
                output.push_str("\x1b[34m~\x1b[0m");
            }

            if i < visible_height - 1 {
                output.push_str("\r\n");
            }
        }

        output.push_str(&format!("\x1b[{};1H", self.terminal_height - 1));
        output.push_str("\x1b[100m\x1b[37m");

        let modified_indicator = if self.modified { "*" } else { "" };
        let status = if self.checking_urls {
            format!(
                " {}{}  row: {} col: {}  checking... {}/{} ({} missing)  ",
                self.filename,
                modified_indicator,
                self.cursor_row + 1,
                self.cursor_col + 1,
                self.check_progress.0,
                self.check_progress.1,
                self.missing_emotes.len()
            )
        } else {
            format!(
                " {}{}  row: {} col: {}  dup: {} 404: {}  F1: help | F2: save | F10: quit  ",
                self.filename,
                modified_indicator,
                self.cursor_row + 1,
                self.cursor_col + 1,
                self.duplicates.len(),
                self.missing_emotes.len()
            )
        };

        let status_display = if status.len() > self.terminal_width {
            &status[..self.terminal_width]
        } else {
            &status
        };

        output.push_str(status_display);

        for _ in status_display.len()..self.terminal_width {
            output.push(' ');
        }

        output.push_str("\x1b[0m");

        output.push_str(&format!("\x1b[{};1H\x1b[2K", self.terminal_height));
        let msg_display = if self.status_message.len() > self.terminal_width {
            &self.status_message[..self.terminal_width]
        } else {
            &self.status_message
        };
        output.push_str(msg_display);

        let screen_row = self.cursor_row.saturating_sub(self.scroll_offset) + 1;
        output.push_str(&format!("\x1b[{};{}H", screen_row, self.cursor_col + 6));

        stdout.write_all(output.as_bytes())?;
        stdout.flush()?;

        Ok(())
    }

    fn show_help(&mut self) {
        self.status_message = "F2: save | F10: quit | F3: remove dupes | F4: check 404 | F5: remove missing | F6: load".to_string();
    }

    async fn prompt_load(&mut self) -> io::Result<()> {
        queue!(
            stdout(),
            cursor::MoveTo(0, (self.terminal_height - 1) as u16)
        )?;
        queue!(stdout(), terminal::Clear(ClearType::CurrentLine))?;
        queue!(
            stdout(),
            Print("enter url or file path (enter for default): ")
        )?;
        stdout().flush()?;

        terminal::disable_raw_mode()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        terminal::enable_raw_mode()?;

        let input = input.trim();
        if input.is_empty() {
            if let Err(e) = self.load_from_url(DEFAULT_URL).await {
                self.status_message = format!("error: {}", e);
            }
        } else if input.starts_with("http") {
            if let Err(e) = self.load_from_url(input).await {
                self.status_message = format!("error: {}", e);
            }
        } else {
            if let Err(e) = self.load_from_file(input) {
                self.status_message = format!("error: {}", e);
            }
        }

        Ok(())
    }
    async fn run(&mut self) -> io::Result<()> {
        loop {
            self.refresh_screen()?;

            if event::poll(Duration::from_millis(50))? {
                if let Event::Key(KeyEvent {
                    code,
                    modifiers,
                    kind,
                    ..
                }) = event::read()?
                {
                    if kind != event::KeyEventKind::Press {
                        continue;
                    }

                    match code {
                        KeyCode::F(1) => {
                            self.show_help();
                        }
                        KeyCode::F(2) => {
                            if let Err(e) = self.save_file() {
                                self.status_message = format!("save error: {}", e);
                            }
                        }
                        KeyCode::F(3) => {
                            self.remove_duplicates();
                        }
                        KeyCode::F(4) => {
                            if !self.checking_urls {
                                if let Err(e) = self.check_missing_emotes().await {
                                    self.status_message = format!("error checking urls: {}", e);
                                }
                            }
                        }
                        KeyCode::F(5) => {
                            if !self.missing_emotes.is_empty() {
                                self.remove_missing_emotes();
                            } else {
                                self.status_message =
                                    "no missing emotes to remove,  press F4 to check first!!11!"
                                        .to_string();
                            }
                        }
                        KeyCode::F(6) => {
                            self.prompt_load().await?;
                        }
                        KeyCode::F(10) => break,
                        KeyCode::Char('z') if modifiers.contains(KeyModifiers::CONTROL) => {
                            self.undo();
                        }
                        KeyCode::Char('y') if modifiers.contains(KeyModifiers::CONTROL) => {
                            self.redo();
                        }
                        KeyCode::Char(c)
                            if !modifiers.contains(KeyModifiers::CONTROL)
                                && !modifiers.contains(KeyModifiers::ALT) =>
                        {
                            self.insert_char(c);
                        }
                        KeyCode::Enter => {
                            self.insert_newline();
                        }
                        KeyCode::Backspace => {
                            self.delete_char();
                        }
                        KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right => {
                            let prev_row = self.cursor_row;
                            let prev_col = self.cursor_col;

                            self.move_cursor(code);

                            if prev_row != self.cursor_row || prev_col != self.cursor_col {
                                self.refresh_screen()?;
                            }
                        }
                        KeyCode::Esc => {
                            self.status_message = "press F10 to quit".to_string();
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> io::Result<()> {
    terminal::enable_raw_mode()?;
    execute!(stdout(), terminal::EnterAlternateScreen)?;

    let mut editor = Moaremotes::new()?;

    if std::path::Path::new("emotes.json").exists() {
        if let Err(e) = editor.load_from_file("emotes.json") {
            editor.status_message = format!("warning: {}", e);
        }
    } else {
        editor.status_message =
            "no emotes.json found.. press F6 to load from url or file".to_string();
    }

    editor.run().await?;

    terminal::disable_raw_mode()?;
    execute!(stdout(), terminal::LeaveAlternateScreen)?;

    Ok(())
}
