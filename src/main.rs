use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result};
use dashmap::DashMap;
use mago_database::file::{File, FileType};
use mago_syntax::ast::Program;
use mago_syntax::ast::ast::Argument;
use mago_syntax::ast::ast::Attribute;
use mago_syntax::ast::ast::Class;
use mago_syntax::ast::ast::ClassLikeMember;
use mago_syntax::ast::ast::Expression;
use mago_syntax::ast::ast::Literal;
use mago_syntax::ast::ast::Method;
use mago_syntax::ast::ast::Statement;
use mago_syntax::ast::sequence::Sequence;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::Value as JsonValue;
use serde_yaml::Value;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{Mutex, RwLock};
use tower_lsp::jsonrpc::Result as LspResult;
use tower_lsp::lsp_types::notification::Progress;
use tower_lsp::lsp_types::request::WorkDoneProgressCreate;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use walkdir::WalkDir;

#[derive(Clone, Debug)]
struct StepDef {
    text: String,
    source: PathBuf,
    class: Option<String>,
    kind: StepKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StepKind {
    Given,
    When,
    Then,
    And,
    But,
    Other,
}

struct State {
    root: RwLock<PathBuf>,
    steps: RwLock<Vec<StepDef>>,
    open_docs: DashMap<Url, String>,
    watcher: Mutex<Option<RecommendedWatcher>>,
    rebuild_lock: Mutex<()>,
    progress_id: AtomicU32,
}

impl Default for State {
    fn default() -> Self {
        Self {
            root: RwLock::new(PathBuf::from(".")),
            steps: RwLock::new(Vec::new()),
            open_docs: DashMap::new(),
            watcher: Mutex::new(None),
            rebuild_lock: Mutex::new(()),
            progress_id: AtomicU32::new(1),
        }
    }
}

#[derive(Clone)]
struct Backend {
    client: Client,
    state: Arc<State>,
}

impl Backend {
    async fn rebuild_index(&self) {
        let _guard = self.state.rebuild_lock.lock().await;
        let root = { self.state.root.read().await.clone() };
        let client = self.client.clone();
        let token =
            NumberOrString::Number(self.state.progress_id.fetch_add(1, Ordering::SeqCst) as i32);
        let _ = self
            .client
            .send_request::<WorkDoneProgressCreate>(WorkDoneProgressCreateParams {
                token: token.clone(),
            })
            .await;
        let _ = self
            .client
            .send_notification::<Progress>(ProgressParams {
                token: token.clone(),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(
                    WorkDoneProgressBegin {
                        title: "Behat index".to_string(),
                        cancellable: Some(false),
                        message: Some("Parsing contexts and steps".to_string()),
                        percentage: None,
                    },
                )),
            })
            .await;

        let (progress_tx, mut progress_rx) =
            tokio::sync::mpsc::unbounded_channel::<ProgressEvent>();
        let progress_client = self.client.clone();
        let progress_token = token.clone();
        let progress_handle = tokio::spawn(async move {
            let mut contexts_done = 0usize;
            let mut steps_done = 0usize;
            let mut contexts_total = None;
            while let Some(event) = progress_rx.recv().await {
                match event {
                    ProgressEvent::ContextParsed { total } => {
                        contexts_done += 1;
                        contexts_total = Some(total);
                    }
                    ProgressEvent::StepParsed => {
                        steps_done += 1;
                    }
                }
                let total = contexts_total
                    .map(|t| format!("{contexts_done}/{t}"))
                    .unwrap_or_else(|| format!("{contexts_done}"));
                let _ = progress_client
                    .send_notification::<Progress>(ProgressParams {
                        token: progress_token.clone(),
                        value: ProgressParamsValue::WorkDone(WorkDoneProgress::Report(
                            WorkDoneProgressReport {
                                cancellable: Some(false),
                                message: Some(format!("contexts {total}, steps {steps_done}")),
                                percentage: None,
                            },
                        )),
                    })
                    .await;
            }
        });

        let steps = match tokio::task::spawn_blocking(move || build_index(&root, progress_tx)).await
        {
            Ok(Ok(steps)) => steps,
            Ok(Err(err)) => {
                let _ = client
                    .log_message(MessageType::ERROR, format!("Index build failed: {err}"))
                    .await;
                let _ = client
                    .send_notification::<Progress>(ProgressParams {
                        token,
                        value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(
                            WorkDoneProgressEnd {
                                message: Some("Index build failed".to_string()),
                            },
                        )),
                    })
                    .await;
                return;
            }
            Err(err) => {
                let _ = client
                    .log_message(
                        MessageType::ERROR,
                        format!("Index build task failed: {err}"),
                    )
                    .await;
                let _ = client
                    .send_notification::<Progress>(ProgressParams {
                        token,
                        value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(
                            WorkDoneProgressEnd {
                                message: Some("Index build failed".to_string()),
                            },
                        )),
                    })
                    .await;
                return;
            }
        };
        let _ = progress_handle.await;
        let _ = self
            .client
            .send_notification::<Progress>(ProgressParams {
                token,
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(WorkDoneProgressEnd {
                    message: Some(format!("Index built ({} steps)", steps.len())),
                })),
            })
            .await;
        *self.state.steps.write().await = steps;
    }

    async fn start_watcher(&self) -> Result<()> {
        let root = { self.state.root.read().await.clone() };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<notify::Result<Event>>();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })?;
        watcher.watch(&root, RecursiveMode::Recursive)?;
        *self.state.watcher.lock().await = Some(watcher);

        let state = self.state.clone();
        let client = self.client.clone();
        tokio::spawn(async move {
            while let Some(res) = rx.recv().await {
                let event = match res {
                    Ok(event) => event,
                    Err(err) => {
                        let _ = client
                            .log_message(MessageType::ERROR, format!("Watcher error: {err}"))
                            .await;
                        continue;
                    }
                };
                if should_rebuild_for_event(&event) {
                    let backend = Backend {
                        client: client.clone(),
                        state: state.clone(),
                    };
                    backend.rebuild_index().await;
                }
            }
        });
        Ok(())
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> LspResult<InitializeResult> {
        let root = params
            .root_uri
            .and_then(|uri| uri.to_file_path().ok())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        *self.state.root.write().await = root;
        if let Err(err) = self.start_watcher().await {
            let _ = self
                .client
                .log_message(
                    MessageType::ERROR,
                    format!("Failed to start watcher: {err}"),
                )
                .await;
        }
        self.rebuild_index().await;

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![" ".to_string(), "/".to_string()]),
                    ..CompletionOptions::default()
                }),
                ..ServerCapabilities::default()
            },
            ..InitializeResult::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        let _ = self
            .client
            .log_message(MessageType::INFO, "behat-lsp initialized")
            .await;
    }

    async fn shutdown(&self) -> LspResult<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.state
            .open_docs
            .insert(params.text_document.uri, params.text_document.text);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        if let Some(change) = params.content_changes.into_iter().next() {
            self.state
                .open_docs
                .insert(params.text_document.uri, change.text);
        }
    }

    async fn did_save(&self, _params: DidSaveTextDocumentParams) {
        self.rebuild_index().await;
    }

    async fn completion(&self, params: CompletionParams) -> LspResult<Option<CompletionResponse>> {
        let token =
            NumberOrString::Number(self.state.progress_id.fetch_add(1, Ordering::SeqCst) as i32);
        let _ = self
            .client
            .send_request::<WorkDoneProgressCreate>(WorkDoneProgressCreateParams {
                token: token.clone(),
            })
            .await;
        let _ = self
            .client
            .send_notification::<Progress>(ProgressParams {
                token: token.clone(),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(
                    WorkDoneProgressBegin {
                        title: "Behat completions".to_string(),
                        cancellable: Some(false),
                        message: Some("Preparing completions".to_string()),
                        percentage: None,
                    },
                )),
            })
            .await;
        let replace_range = self
            .state
            .open_docs
            .get(&params.text_document_position.text_document.uri)
            .and_then(|text| {
                word_replace_range(text.as_str(), params.text_document_position.position)
            });
        let steps = self.state.steps.read().await.clone();
        let items: Vec<CompletionItem> = steps
            .into_iter()
            .map(|step| {
                let keyword = step_kind_keyword(step.kind);
                let (plain, snippet) = build_step_snippet(&step.text);
                let label = format!("{keyword} {plain}");
                let new_text = format!("{keyword} {snippet}");
                let mut item = CompletionItem {
                    label,
                    kind: Some(step_kind_to_completion_kind(step.kind)),
                    detail: Some(format_step_detail(&step)),
                    insert_text_format: Some(InsertTextFormat::SNIPPET),
                    ..CompletionItem::default()
                };
                if let Some(range) = replace_range {
                    item.text_edit = Some(CompletionTextEdit::Edit(TextEdit { range, new_text }));
                } else {
                    item.insert_text = Some(new_text);
                }
                item
            })
            .collect();
        let _ = self
            .client
            .log_message(
                MessageType::INFO,
                format!("behat-lsp: completions ready ({})", items.len()),
            )
            .await;
        let _ = self
            .client
            .send_notification::<Progress>(ProgressParams {
                token,
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(WorkDoneProgressEnd {
                    message: Some(format!("{} completion(s)", items.len())),
                })),
            })
            .await;
        Ok(Some(CompletionResponse::List(CompletionList {
            is_incomplete: true,
            items,
        })))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let state = Arc::new(State::default());
    let (service, socket) = LspService::new(|client| Backend { client, state });
    Server::new(stdin, stdout, socket).serve(service).await;
    Ok(())
}

fn should_rebuild_for_event(event: &Event) -> bool {
    if matches!(
        event.kind,
        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
    ) {
        for path in &event.paths {
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name == "behat.yaml" || name == "composer.lock" || name == "composer.json"
                })
            {
                return true;
            }
            if path.extension().is_some_and(|ext| ext == "php") {
                return true;
            }
        }
    }
    false
}

#[derive(Debug, Clone, Copy)]
enum ProgressEvent {
    ContextParsed { total: usize },
    StepParsed,
}

fn build_index(root: &Path, progress: UnboundedSender<ProgressEvent>) -> Result<Vec<StepDef>> {
    let behat_path = root.join("behat.yaml");
    let contexts = read_contexts(&behat_path).unwrap_or_default();
    if contexts.is_empty() {
        return Ok(Vec::new());
    }

    let context_set: HashSet<String> = contexts.iter().map(|c| normalize_class_name(c)).collect();
    let composer_locks = find_composer_locks(root);
    let psr4 = build_psr4_map(&composer_locks)?;
    let total_contexts = context_set.len();

    let mut context_paths: HashMap<PathBuf, HashSet<String>> = HashMap::new();
    for context in &context_set {
        let paths = resolve_context_paths(context, root, &psr4);
        for path in paths {
            context_paths
                .entry(path)
                .or_default()
                .insert(context.to_string());
        }
        let _ = progress.send(ProgressEvent::ContextParsed {
            total: total_contexts,
        });
    }

    let mut steps = Vec::new();
    for (path, contexts_for_path) in context_paths {
        let parsed = match parse_php_file(&path, root) {
            Ok(parsed) => parsed,
            Err(_) => continue,
        };
        let mut matched_any = false;
        for class in &parsed.classes {
            let fqcn = normalize_class_name(&class.fqcn);
            if contexts_for_path.contains(&fqcn) {
                matched_any = true;
                for step in &class.steps {
                    steps.push(StepDef {
                        text: step.text.clone(),
                        source: path.clone(),
                        class: Some(class.fqcn.clone()),
                        kind: step.kind,
                    });
                    let _ = progress.send(ProgressEvent::StepParsed);
                }
            }
        }
        if !matched_any {
            for class in &parsed.classes {
                for step in &class.steps {
                    steps.push(StepDef {
                        text: step.text.clone(),
                        source: path.clone(),
                        class: Some(class.fqcn.clone()),
                        kind: step.kind,
                    });
                    let _ = progress.send(ProgressEvent::StepParsed);
                }
            }
        }
    }

    Ok(steps)
}

#[derive(Debug)]
struct ParsedFile {
    classes: Vec<ParsedClass>,
}

#[derive(Debug)]
struct ParsedClass {
    fqcn: String,
    steps: Vec<ParsedStep>,
}

#[derive(Debug)]
struct ParsedStep {
    text: String,
    kind: StepKind,
}

fn parse_php_file(path: &Path, root: &Path) -> Result<ParsedFile> {
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let name = path
        .strip_prefix(root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string_lossy().to_string());
    let file = File::new(
        Cow::Owned(name),
        FileType::Host,
        Some(path.to_path_buf()),
        Cow::Owned(contents),
    );
    let arena = bumpalo::Bump::new();
    let program = mago_syntax::parser::parse_file(&arena, &file);
    let mut classes = Vec::new();
    collect_classes(program, None, &mut classes);
    Ok(ParsedFile { classes })
}

fn collect_classes(program: &Program<'_>, namespace: Option<String>, out: &mut Vec<ParsedClass>) {
    collect_classes_from_statements(&program.statements, namespace, out);
}

fn collect_classes_from_statements(
    statements: &Sequence<'_, Statement<'_>>,
    namespace: Option<String>,
    out: &mut Vec<ParsedClass>,
) {
    for stmt in statements.iter() {
        match stmt {
            Statement::Namespace(ns) => {
                let ns_name = ns.name.as_ref().map(|id| id.value().to_string());
                collect_classes_from_statements(ns.statements(), ns_name, out);
            }
            Statement::Class(class) => {
                let steps = collect_steps_from_class(class);
                let fqcn = qualify_class_name(namespace.as_deref(), class.name.value);
                out.push(ParsedClass { fqcn, steps });
            }
            _ => {}
        }
    }
}

fn qualify_class_name(namespace: Option<&str>, class_name: &str) -> String {
    match namespace {
        Some(ns) if !ns.is_empty() => format!("{ns}\\{class_name}"),
        _ => class_name.to_string(),
    }
}

fn collect_steps_from_class(class: &Class<'_>) -> Vec<ParsedStep> {
    let mut steps = Vec::new();
    for member in class.members.iter() {
        if let ClassLikeMember::Method(method) = member {
            steps.extend(collect_steps_from_method(method));
        }
    }
    steps
}

fn collect_steps_from_method(method: &Method<'_>) -> Vec<ParsedStep> {
    let mut steps = Vec::new();
    for attr_list in method.attribute_lists.iter() {
        for attr in attr_list.attributes.nodes.iter() {
            if let Some(kind) = step_kind_from_attribute(attr)
                && let Some(text) = extract_first_string_argument(attr)
            {
                steps.push(ParsedStep { text, kind });
            }
        }
    }
    steps
}

fn step_kind_from_attribute(attr: &Attribute<'_>) -> Option<StepKind> {
    let name = attr.name.last_segment();
    let kind = match name {
        "Given" => StepKind::Given,
        "When" => StepKind::When,
        "Then" => StepKind::Then,
        "And" => StepKind::And,
        "But" => StepKind::But,
        _ => StepKind::Other,
    };
    if kind == StepKind::Other {
        None
    } else {
        Some(kind)
    }
}

fn extract_first_string_argument(attr: &Attribute<'_>) -> Option<String> {
    let argument_list = attr.argument_list.as_ref()?;
    let arg = argument_list.arguments.first()?;
    match arg {
        Argument::Positional(positional) => match positional.value {
            Expression::Literal(Literal::String(lit)) => {
                let text = lit.value.unwrap_or(lit.raw);
                Some(text.to_string())
            }
            _ => None,
        },
        _ => None,
    }
}

fn step_kind_to_completion_kind(kind: StepKind) -> CompletionItemKind {
    match kind {
        StepKind::Given => CompletionItemKind::EVENT,
        StepKind::Then => CompletionItemKind::KEYWORD,
        StepKind::When => CompletionItemKind::FUNCTION,
        StepKind::And => CompletionItemKind::FIELD,
        StepKind::But => CompletionItemKind::FIELD,
        StepKind::Other => CompletionItemKind::TEXT,
    }
}

fn step_kind_keyword(kind: StepKind) -> &'static str {
    match kind {
        StepKind::Given => "Given",
        StepKind::When => "When",
        StepKind::Then => "Then",
        StepKind::And => "And",
        StepKind::But => "But",
        StepKind::Other => "Given",
    }
}

fn format_step_detail(step: &StepDef) -> String {
    match &step.class {
        Some(class) => format!("{class} ({})", step.source.display()),
        None => step.source.display().to_string(),
    }
}

fn build_step_snippet(text: &str) -> (String, String) {
    let (plain, snippet, _) = build_step_snippet_inner(text, 1, true);
    (plain, snippet)
}

fn build_step_snippet_inner(
    text: &str,
    start_index: usize,
    strip_regex: bool,
) -> (String, String, usize) {
    let mut raw = text.to_string();
    if strip_regex && let Some(stripped) = strip_regex_wrappers(&raw) {
        raw = stripped;
    }

    let mut plain = String::new();
    let mut snippet = String::new();
    let mut index = start_index;
    let mut chars = raw.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == ':' {
            let mut name = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_ascii_alphanumeric() || c == '_' {
                    name.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
            if name.is_empty() {
                plain.push(ch);
                snippet.push(ch);
            } else {
                plain.push_str(&format!("<{name}>"));
                snippet.push_str(&format!("${{{index}:{name}}}"));
                index += 1;
            }
            continue;
        }

        if ch == '(' {
            let mut depth = 1;
            let mut inner = String::new();
            for c in chars.by_ref() {
                if c == '(' {
                    depth += 1;
                } else if c == ')' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                inner.push(c);
            }

            let trimmed = inner.trim();
            if is_simple_regex_placeholder(trimmed) {
                plain.push_str("<param>");
                snippet.push_str(&format!("${{{index}:param}}"));
                index += 1;
            } else {
                let (inner_plain, inner_snippet, new_index) =
                    build_step_snippet_inner(trimmed, index, false);
                plain.push_str(&inner_plain);
                snippet.push_str(&inner_snippet);
                index = new_index;
            }
            continue;
        }

        plain.push(ch);
        snippet.push(ch);
    }

    (plain, snippet, index)
}

fn strip_regex_wrappers(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.len() < 2 {
        return None;
    }
    let mut chars = trimmed.chars();
    let first = chars.next()?;
    let last = trimmed.chars().last()?;
    if first == '/' && last == '/' {
        return Some(trimmed[1..trimmed.len() - 1].to_string());
    }
    if (first == '/' || first == '#') && last == first {
        return Some(trimmed[1..trimmed.len() - 1].to_string());
    }
    None
}

fn is_simple_regex_placeholder(text: &str) -> bool {
    let t = text.trim();
    matches!(
        t,
        ".*" | ".+" | "[^)]*" | "[^\"]*" | "[^']*" | "[^ ]*" | "[^/]*"
    ) || t.starts_with("?:")
        || t.starts_with("?P<")
}

fn word_replace_range(text: &str, position: Position) -> Option<Range> {
    let mut current_line = None;
    for (idx, line) in text.split('\n').enumerate() {
        if idx == position.line as usize {
            current_line = Some(line);
            break;
        }
    }
    let line = current_line?;
    let col = position.character as usize;
    if col > line.len() {
        return None;
    }
    let bytes = line.as_bytes();
    let mut start = col;
    while start > 0 {
        let b = bytes[start - 1];
        if b.is_ascii_whitespace() {
            break;
        }
        start -= 1;
    }
    Some(Range {
        start: Position {
            line: position.line,
            character: start as u32,
        },
        end: position,
    })
}

fn normalize_class_name(name: &str) -> String {
    name.trim_start_matches('\\').to_string()
}

#[derive(Clone, Debug)]
struct Psr4Mapping {
    prefix: String,
    base: PathBuf,
}

fn build_psr4_map(lock_paths: &[PathBuf]) -> Result<Vec<Psr4Mapping>> {
    let mut mappings = Vec::new();
    for lock_path in lock_paths {
        let root = lock_path.parent().unwrap_or_else(|| Path::new("."));
        let contents = std::fs::read_to_string(lock_path)
            .with_context(|| format!("read {}", lock_path.display()))?;
        let value: JsonValue = serde_json::from_str(&contents)
            .with_context(|| format!("parse {}", lock_path.display()))?;
        if lock_path.file_name().and_then(|n| n.to_str()) == Some("composer.json") {
            collect_psr4_from_package(value.get("autoload"), root, &mut mappings);
            collect_psr4_from_package(value.get("autoload-dev"), root, &mut mappings);
        } else {
            collect_psr4_from_lock(&value, root, &mut mappings);
        }
    }
    mappings.sort_by(|a, b| b.prefix.len().cmp(&a.prefix.len()));
    Ok(mappings)
}

fn collect_psr4_from_lock(value: &JsonValue, root: &Path, out: &mut Vec<Psr4Mapping>) {
    for section in ["packages", "packages-dev"] {
        let packages = value.get(section).and_then(|v| v.as_array());
        if let Some(packages) = packages {
            for pkg in packages {
                let name = match pkg.get("name").and_then(|v| v.as_str()) {
                    Some(name) => name,
                    None => continue,
                };
                let base = root.join("vendor").join(name);
                collect_psr4_from_package(pkg.get("autoload"), &base, out);
                collect_psr4_from_package(pkg.get("autoload-dev"), &base, out);
            }
        }
    }
}

fn collect_psr4_from_package(
    autoload: Option<&JsonValue>,
    base: &Path,
    out: &mut Vec<Psr4Mapping>,
) {
    let psr4 = autoload
        .and_then(|v| v.get("psr-4"))
        .and_then(|v| v.as_object());
    if let Some(psr4) = psr4 {
        for (prefix, paths) in psr4 {
            let prefix = prefix.trim_start_matches('\\').to_string();
            let mut add_path = |path: &str| {
                out.push(Psr4Mapping {
                    prefix: prefix.clone(),
                    base: base.join(path),
                });
            };
            match paths {
                JsonValue::String(path) => add_path(path),
                JsonValue::Array(items) => {
                    for item in items {
                        if let Some(path) = item.as_str() {
                            add_path(path);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn resolve_context_paths(context: &str, root: &Path, mappings: &[Psr4Mapping]) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let class = normalize_class_name(context);

    for mapping in mappings {
        if class.starts_with(&mapping.prefix) {
            let rest = class[mapping.prefix.len()..].trim_start_matches('\\');
            let rel = rest.replace('\\', "/");
            let candidate = mapping.base.join(rel).with_extension("php");
            if candidate.exists() {
                candidates.push(candidate);
            }
        }
    }

    if candidates.is_empty() {
        let rel = class.replace('\\', "/");
        let direct = root.join(&rel).with_extension("php");
        if direct.exists() {
            candidates.push(direct);
        }
        let src = root.join("src").join(&rel).with_extension("php");
        if src.exists() {
            candidates.push(src);
        }
    }

    candidates.sort();
    candidates.dedup();
    candidates
}

fn find_composer_locks(root: &Path) -> Vec<PathBuf> {
    WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_entry(|e| !is_ignored_dir(e))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            let name = e.file_name().to_string_lossy();
            name == "composer.lock" || name == "composer.json"
        })
        .map(|e| e.path().to_path_buf())
        .collect()
}

fn read_contexts(path: &Path) -> Result<Vec<String>> {
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let value: Value = serde_yaml::from_str(&contents).context("parse behat.yaml")?;
    let mut out = Vec::new();
    collect_contexts(&value, &mut out);
    Ok(out)
}

fn collect_contexts(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Mapping(map) => {
            for (k, v) in map {
                if let Value::String(key) = k
                    && key == "contexts"
                    && let Value::Sequence(items) = v
                {
                    for item in items {
                        match item {
                            Value::String(name) => out.push(name.clone()),
                            Value::Mapping(m) => {
                                for (mk, _) in m {
                                    if let Value::String(name) = mk {
                                        out.push(name.clone());
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                collect_contexts(v, out);
            }
        }
        Value::Sequence(items) => {
            for item in items {
                collect_contexts(item, out);
            }
        }
        _ => {}
    }
}

fn is_ignored_dir(entry: &walkdir::DirEntry) -> bool {
    if !entry.file_type().is_dir() {
        return false;
    }
    let name = entry.file_name().to_string_lossy();
    matches!(name.as_ref(), ".git" | "vendor" | "node_modules" | "target")
}
