use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use ruby_prism::{self, Node, Visit};

use crate::types::GemEntry;

const DEFAULT_DEVELOPMENT_GROUP: &str = "development";

/// Parse a Gemfile using ruby-prism AST and extract gem declarations.
pub fn parse_gemfile(dir: &Path) -> Vec<GemEntry> {
    let gemfile_path = dir.join("Gemfile");
    if !gemfile_path.exists() {
        eprintln!("Error: No Gemfile found in {}", dir.display());
        std::process::exit(1);
    }

    parse_gemfiles(dir, &[gemfile_path])
}

/// Parse an explicit list of Gemfiles. Each Gemfile is evaluated in the order given,
/// and `eval_gemfile` directives are evaluated inline at their declaration site so
/// that source order, group context and platform context are preserved.
pub fn parse_gemfiles(dir: &Path, gemfile_paths: &[PathBuf]) -> Vec<GemEntry> {
    let mut ctx = ParseCtx {
        root: lexical_normalize(dir),
        gems: Vec::new(),
        active: Vec::new(),
        evaluations: 0,
    };

    for path in gemfile_paths {
        let abs = if path.is_absolute() {
            path.clone()
        } else {
            dir.join(path)
        };
        eval_gemfile_file(&mut ctx, &abs, &[], &[]);
    }

    merge_duplicate_gems(ctx.gems)
}

/// Upper bound on evaluated Gemfiles, guarding against pathological
/// `eval_gemfile` graphs (a deep diamond can otherwise expand exponentially).
const MAX_GEMFILE_EVALUATIONS: usize = 1024;

/// Shared state threaded through nested `eval_gemfile` evaluation.
struct ParseCtx {
    /// Approximation of Bundler's working directory, used for `File.expand_path`
    /// calls that have no explicit base.
    root: PathBuf,
    gems: Vec<PendingGem>,
    /// Canonicalized paths of the Gemfiles currently being evaluated. Only the
    /// active recursion stack is tracked, so the same Gemfile evaluated again
    /// under a different group/platform context still contributes its context.
    active: Vec<PathBuf>,
    evaluations: usize,
}

/// A gem declaration plus the provenance Bundler needs to resolve duplicates.
struct PendingGem {
    entry: GemEntry,
    /// True when the entry came from a gemspec's `add_development_dependency`.
    gemspec_dev_dep: bool,
}

fn eval_gemfile_file(ctx: &mut ParseCtx, path: &Path, groups: &[String], platforms: &[String]) {
    let path = lexical_normalize(path);
    if !path.is_file() {
        return;
    }

    let key = dedupe_key(&path);
    if ctx.active.contains(&key) {
        eprintln!(
            "Warning: eval_gemfile cycle detected at {} — skipping re-evaluation",
            path.display()
        );
        return;
    }
    if ctx.evaluations >= MAX_GEMFILE_EVALUATIONS {
        eprintln!(
            "Warning: eval_gemfile limit of {} files reached — skipping {}",
            MAX_GEMFILE_EVALUATIONS,
            path.display()
        );
        return;
    }
    ctx.evaluations += 1;

    let source = match std::fs::read(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Warning: Could not read Gemfile {}: {}", path.display(), e);
            return;
        }
    };

    let parse_result = ruby_prism::parse(&source);
    let base_dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let mut visitor = GemfileVisitor {
        ctx,
        source: &source,
        gemfile_path: path.clone(),
        base_dir,
        current_groups: groups.to_vec(),
        current_platforms: platforms.to_vec(),
    };
    visitor.ctx.active.push(key);
    visitor.visit(&parse_result.node());
    visitor.ctx.active.pop();
}

struct GemfileVisitor<'a, 's> {
    ctx: &'a mut ParseCtx,
    source: &'s [u8],
    gemfile_path: PathBuf,
    base_dir: PathBuf,
    current_groups: Vec<String>,
    current_platforms: Vec<String>,
}

impl GemfileVisitor<'_, '_> {
    fn handle_gem(&mut self, node: &ruby_prism::CallNode<'_>) {
        let args = match node.arguments() {
            Some(a) => a,
            None => return,
        };
        let arg_list: Vec<Node<'_>> = args.arguments().iter().collect();
        if arg_list.is_empty() {
            return;
        }

        // First arg should be the gem name (string)
        let name = match extract_string_value(&arg_list[0], self.source) {
            Some(n) => n,
            None => return,
        };

        let mut require_false = false;
        let mut require_paths: Vec<String> = Vec::new();
        let mut groups: Vec<String> = self.current_groups.clone();
        let mut platforms: Vec<String> = self.current_platforms.clone();
        let mut source = None;

        // Check remaining args for keyword hash
        for arg in &arg_list[1..] {
            if let Some(hash) = arg.as_keyword_hash_node() {
                for elem in hash.elements().iter() {
                    if let Some(assoc) = elem.as_assoc_node() {
                        let key_name = extract_key_name(&assoc.key(), self.source);
                        match key_name.as_deref() {
                            Some("require") => {
                                if assoc.value().as_false_node().is_some() {
                                    require_false = true;
                                } else if let Some(s) =
                                    extract_string_value(&assoc.value(), self.source)
                                {
                                    require_paths.push(s);
                                } else if let Some(arr) = assoc.value().as_array_node() {
                                    if arr.elements().is_empty() {
                                        require_false = true;
                                    }
                                    for el in arr.elements().iter() {
                                        if let Some(s) = extract_string_value(&el, self.source) {
                                            require_paths.push(s);
                                        }
                                    }
                                }
                            }
                            Some("group") | Some("groups") => {
                                if let Some(sym) = extract_symbol_value(&assoc.value(), self.source)
                                {
                                    groups.push(sym);
                                } else if let Some(arr) = assoc.value().as_array_node() {
                                    for el in arr.elements().iter() {
                                        if let Some(s) = extract_symbol_value(&el, self.source) {
                                            groups.push(s);
                                        }
                                    }
                                }
                            }
                            Some("platform") | Some("platforms") => {
                                if let Some(sym) = extract_symbol_value(&assoc.value(), self.source)
                                {
                                    platforms.push(sym);
                                } else if let Some(arr) = assoc.value().as_array_node() {
                                    for el in arr.elements().iter() {
                                        if let Some(s) = extract_symbol_value(&el, self.source) {
                                            platforms.push(s);
                                        }
                                    }
                                }
                            }
                            Some("path") => {
                                if let Some(s) = extract_string_value(&assoc.value(), self.source) {
                                    let path = PathBuf::from(s);
                                    let path = if path.is_absolute() {
                                        path
                                    } else {
                                        self.base_dir.join(path)
                                    };
                                    source = Some(crate::types::GemSource::Path(path));
                                }
                            }
                            Some("git") => {
                                if let Some(s) = extract_string_value(&assoc.value(), self.source) {
                                    source = Some(crate::types::GemSource::Git(s));
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        if groups.is_empty() {
            groups.push("default".to_string());
        }

        self.ctx.gems.push(PendingGem {
            entry: GemEntry {
                name,
                groups,
                platforms,
                require_paths,
                module_names: Vec::new(),
                require_false,
                source,
            },
            gemspec_dev_dep: false,
        });
    }

    fn extract_group_names(&self, node: &ruby_prism::CallNode<'_>) -> Vec<String> {
        let mut groups = Vec::new();
        if let Some(args) = node.arguments() {
            for arg in args.arguments().iter() {
                if let Some(sym) = extract_symbol_value(&arg, self.source) {
                    groups.push(sym);
                }
            }
        }
        groups
    }

    fn extract_platform_names(&self, node: &ruby_prism::CallNode<'_>) -> Vec<String> {
        let mut platforms = Vec::new();
        if let Some(args) = node.arguments() {
            for arg in args.arguments().iter() {
                if let Some(sym) = extract_symbol_value(&arg, self.source) {
                    platforms.push(sym);
                }
            }
        }
        platforms
    }

    /// Evaluate `eval_gemfile <expr>` inline, preserving group/platform context.
    fn handle_eval_gemfile(&mut self, node: &ruby_prism::CallNode<'_>) {
        let first = node
            .arguments()
            .and_then(|args| args.arguments().iter().next());
        let first = match first {
            Some(f) => f,
            None => {
                eprintln!(
                    "Warning: eval_gemfile without arguments in {} — ignored",
                    self.gemfile_path.display()
                );
                return;
            }
        };

        let expr = eval_path_expr(&first, &self.gemfile_path, &self.ctx.root, self.source);
        let target = match expr {
            Some(e) => lexical_normalize(&e.to_abs(&self.base_dir)),
            None => {
                eprintln!(
                    "Warning: unsupported eval_gemfile argument in {} — ignored",
                    self.gemfile_path.display()
                );
                return;
            }
        };

        if !target.is_file() {
            eprintln!(
                "Warning: eval_gemfile target {} (from {}) does not exist — ignored",
                target.display(),
                self.gemfile_path.display()
            );
            return;
        }

        let groups = self.current_groups.clone();
        let platforms = self.current_platforms.clone();
        eval_gemfile_file(self.ctx, &target, &groups, &platforms);
    }

    /// Handle a `gemspec` directive, honoring `path:`, `name:` and `development_group:`.
    fn handle_gemspec(&mut self, node: &ruby_prism::CallNode<'_>) {
        let mut rel_path: Option<String> = None;
        let mut name: Option<String> = None;
        let mut development_group = DEFAULT_DEVELOPMENT_GROUP.to_string();

        if let Some(args) = node.arguments() {
            for arg in args.arguments().iter() {
                let hash = match arg.as_keyword_hash_node() {
                    Some(h) => h,
                    None => continue,
                };
                for elem in hash.elements().iter() {
                    let assoc = match elem.as_assoc_node() {
                        Some(a) => a,
                        None => continue,
                    };
                    let key = extract_key_name(&assoc.key(), self.source);
                    match key.as_deref() {
                        Some("path") => match extract_string_value(&assoc.value(), self.source) {
                            Some(p) => rel_path = Some(p),
                            None => eprintln!(
                                "Warning: unsupported gemspec `path:` value in {} — using the Gemfile directory",
                                self.gemfile_path.display()
                            ),
                        },
                        Some("name") => match extract_string_value(&assoc.value(), self.source)
                            .or_else(|| extract_symbol_value(&assoc.value(), self.source))
                        {
                            Some(n) => name = Some(n),
                            None => eprintln!(
                                "Warning: unsupported gemspec `name:` value in {} — ignored",
                                self.gemfile_path.display()
                            ),
                        },
                        Some("development_group") => {
                            match extract_symbol_value(&assoc.value(), self.source)
                                .or_else(|| extract_string_value(&assoc.value(), self.source))
                            {
                                Some(g) => development_group = g,
                                None => eprintln!(
                                    "Warning: unsupported gemspec `development_group:` value in {} — using :{}",
                                    self.gemfile_path.display(),
                                    DEFAULT_DEVELOPMENT_GROUP
                                ),
                            }
                        }
                        Some(other) => eprintln!(
                            "Warning: unsupported gemspec option `{}:` in {} — ignored",
                            other,
                            self.gemfile_path.display()
                        ),
                        None => {}
                    }
                }
            }
        }

        let spec_dir = lexical_normalize(&match rel_path {
            Some(p) => {
                let p = PathBuf::from(p);
                if p.is_absolute() {
                    p
                } else {
                    self.base_dir.join(p)
                }
            }
            None => self.base_dir.clone(),
        });

        let spec = match select_gemspec(&spec_dir, name.as_deref(), &self.gemfile_path) {
            Some(s) => s,
            None => return,
        };

        let self_groups = if self.current_groups.is_empty() {
            vec!["default".to_string()]
        } else {
            self.current_groups.clone()
        };

        // Bundler wraps development dependencies in `group(development_group)`, which
        // nests inside any enclosing group block but does not imply :default.
        let mut dev_groups = self.current_groups.clone();
        if !dev_groups.contains(&development_group) {
            dev_groups.push(development_group);
        }

        register_gemspec(
            self.ctx,
            spec,
            &spec_dir,
            &self_groups,
            &dev_groups,
            &self.current_platforms,
        );
    }
}

impl Visit<'_> for GemfileVisitor<'_, '_> {
    fn visit_call_node(&mut self, node: &ruby_prism::CallNode<'_>) {
        let name = node.name();
        let name_bytes = name.as_slice();

        if name_bytes == b"gem" && node.receiver().is_none() {
            self.handle_gem(node);
        } else if name_bytes == b"group" && node.receiver().is_none() {
            let groups = self.extract_group_names(node);
            let prev_len = self.current_groups.len();
            self.current_groups.extend(groups);
            ruby_prism::visit_call_node(self, node);
            self.current_groups.truncate(prev_len);
            return;
        } else if (name_bytes == b"platforms" || name_bytes == b"platform")
            && node.receiver().is_none()
        {
            let platforms = self.extract_platform_names(node);
            let prev_len = self.current_platforms.len();
            self.current_platforms.extend(platforms);
            ruby_prism::visit_call_node(self, node);
            self.current_platforms.truncate(prev_len);
            return;
        } else if name_bytes == b"eval_gemfile" && node.receiver().is_none() {
            self.handle_eval_gemfile(node);
        } else if name_bytes == b"gemspec" && node.receiver().is_none() {
            self.handle_gemspec(node);
        }

        ruby_prism::visit_call_node(self, node);
    }
}

// --- Path expression evaluation (File.expand_path / __dir__ / __FILE__ / File.join) ---

/// A path literal recovered from the Gemfile AST. Relative values are resolved
/// against the directory of the Gemfile that declared them.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PathExpr {
    Abs(PathBuf),
    Rel(String),
}

impl PathExpr {
    fn to_abs(&self, base: &Path) -> PathBuf {
        match self {
            PathExpr::Abs(p) => p.clone(),
            PathExpr::Rel(r) => base.join(r),
        }
    }

    fn from_string(s: String) -> PathExpr {
        if Path::new(&s).is_absolute() {
            PathExpr::Abs(PathBuf::from(s))
        } else {
            PathExpr::Rel(s)
        }
    }
}

/// Evaluate a small subset of Ruby path expressions used in real Gemfiles.
///
/// Supported: string literals, `__FILE__`, `__dir__`, `File.expand_path(path[, base])`,
/// `File.dirname(expr)` and `File.join(parts...)`. Anything else returns `None` so the
/// caller can warn instead of silently dropping the directive.
fn eval_path_expr(
    node: &Node<'_>,
    gemfile_path: &Path,
    root: &Path,
    source: &[u8],
) -> Option<PathExpr> {
    if let Some(s) = extract_string_value(node, source) {
        return Some(PathExpr::from_string(s));
    }
    if node.as_source_file_node().is_some() {
        // __FILE__ — the path of the Gemfile currently being evaluated.
        return Some(PathExpr::Abs(gemfile_path.to_path_buf()));
    }

    let call = node.as_call_node()?;
    let name = call.name();
    let name_bytes = name.as_slice();

    // __dir__ — the directory containing the Gemfile currently being evaluated.
    if name_bytes == b"__dir__" && call.receiver().is_none() {
        return Some(PathExpr::Abs(gemfile_dir(gemfile_path)));
    }

    if !receiver_is_const(&call, b"File") {
        return None;
    }

    let args: Vec<Node<'_>> = call
        .arguments()
        .map(|a| a.arguments().iter().collect())
        .unwrap_or_default();

    match name_bytes {
        b"expand_path" => {
            let target = eval_path_expr(args.first()?, gemfile_path, root, source)?;
            // `File.expand_path(p, base)` treats `base` as a directory string, so
            // `File.expand_path("../x", __FILE__)` resolves next to the Gemfile.
            let base = match args.get(1) {
                Some(b) => {
                    let expr = eval_path_expr(b, gemfile_path, root, source)?;
                    lexical_normalize(&expr.to_abs(root))
                }
                // No base: Ruby expands against the working directory.
                None => root.to_path_buf(),
            };
            Some(PathExpr::Abs(lexical_normalize(&target.to_abs(&base))))
        }
        b"dirname" => {
            let inner = eval_path_expr(args.first()?, gemfile_path, root, source)?;
            match inner {
                PathExpr::Abs(p) => Some(PathExpr::Abs(
                    p.parent().map(Path::to_path_buf).unwrap_or(p),
                )),
                PathExpr::Rel(r) => {
                    let parent = Path::new(&r)
                        .parent()
                        .map(|p| p.to_string_lossy().to_string())
                        .filter(|p| !p.is_empty())
                        .unwrap_or_else(|| ".".to_string());
                    Some(PathExpr::Rel(parent))
                }
            }
        }
        b"join" => {
            if args.is_empty() {
                return None;
            }
            let mut parts: Vec<String> = Vec::with_capacity(args.len());
            for arg in &args {
                parts.push(match eval_path_expr(arg, gemfile_path, root, source)? {
                    PathExpr::Abs(p) => p.to_string_lossy().to_string(),
                    PathExpr::Rel(r) => r,
                });
            }
            Some(PathExpr::from_string(ruby_file_join(&parts)))
        }
        _ => None,
    }
}

/// Ruby's `File.join`: components are concatenated with a single `/`, and a leading
/// separator on a non-first component does *not* restart the path
/// (`File.join('a', '/b', 'c') == "a/b/c"`).
fn ruby_file_join(parts: &[String]) -> String {
    let mut out = String::new();
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            out.push_str(part);
            continue;
        }
        let trimmed = out.trim_end_matches('/').len();
        out.truncate(trimmed);
        out.push('/');
        out.push_str(part.trim_start_matches('/'));
    }
    out
}

fn gemfile_dir(gemfile_path: &Path) -> PathBuf {
    gemfile_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// True when the call's receiver is the constant `name` (e.g. `File` or `::File`).
fn receiver_is_const(call: &ruby_prism::CallNode<'_>, name: &[u8]) -> bool {
    let recv = match call.receiver() {
        Some(r) => r,
        None => return false,
    };
    if let Some(c) = recv.as_constant_read_node() {
        return c.name().as_slice() == name;
    }
    if let Some(c) = recv.as_constant_path_node() {
        return c.name().map(|n| n.as_slice() == name).unwrap_or(false);
    }
    false
}

/// Normalize `.` and `..` components lexically without touching the filesystem.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir) | Some(Component::Prefix(_)) => {}
                _ => out.push(".."),
            },
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Key used to detect that two different spellings refer to the same Gemfile.
fn dedupe_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| lexical_normalize(path))
}

// --- gemspec handling ---

/// A statically parsed `.gemspec`.
struct ParsedGemspec {
    path: PathBuf,
    /// `spec.name` when it is a literal, otherwise the file name stem.
    name: String,
    development_deps: Vec<String>,
}

/// List the gemspecs of a directory the way Bundler's `{,*}.gemspec` glob does:
/// immediate entries only, no recursion, deterministic order.
fn gemspec_candidates(spec_dir: &Path, gemfile_path: &Path) -> Option<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(spec_dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!(
                "Warning: gemspec directive in {} points at {} which could not be read: {}",
                gemfile_path.display(),
                spec_dir.display(),
                e
            );
            return None;
        }
    };

    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        // `{,*}.gemspec`: `*` does not match a leading dot, so only a bare
        // `.gemspec` qualifies among dotfiles.
        let matches = file_name == ".gemspec"
            || (file_name.ends_with(".gemspec") && !file_name.starts_with('.'));
        if matches && entry.path().is_file() {
            candidates.push(entry.path());
        }
    }
    // Deterministic ordering regardless of readdir order.
    candidates.sort();
    Some(candidates)
}

/// Resolve the gemspec a `gemspec` directive refers to. Like Bundler, `name:` is
/// matched against the *parsed* `spec.name`, not the gemspec's file name, and
/// gemspecs declaring the same name are not treated as ambiguous.
fn select_gemspec(
    spec_dir: &Path,
    name: Option<&str>,
    gemfile_path: &Path,
) -> Option<ParsedGemspec> {
    let candidates = gemspec_candidates(spec_dir, gemfile_path)?;
    let mut parsed: Vec<ParsedGemspec> = candidates
        .iter()
        .filter_map(|p| load_gemspec(p, spec_dir))
        .collect();

    if let Some(name) = name {
        let available: Vec<String> = parsed.iter().map(|p| p.name.clone()).collect();
        parsed.retain(|p| p.name == name);
        if parsed.is_empty() {
            eprintln!(
                "Warning: gemspec directive in {} requested name `{}` but {} declares {} — ignored",
                gemfile_path.display(),
                name,
                spec_dir.display(),
                if available.is_empty() {
                    "no gemspecs".to_string()
                } else {
                    format!("[{}]", available.join(", "))
                }
            );
            return None;
        }
        return parsed.into_iter().next();
    }

    if parsed.is_empty() {
        eprintln!(
            "Warning: gemspec directive in {} found no gemspec in {}",
            gemfile_path.display(),
            spec_dir.display()
        );
        return None;
    }

    // Bundler groups candidates by gem name (and version): several files declaring
    // the same gem are not ambiguous.
    let distinct: Vec<&String> = {
        let mut seen: Vec<&String> = Vec::new();
        for p in &parsed {
            if !seen.contains(&&p.name) {
                seen.push(&p.name);
            }
        }
        seen
    };
    if distinct.len() > 1 {
        let listed: Vec<String> = parsed
            .iter()
            .map(|p| {
                format!(
                    "{} ({})",
                    p.name,
                    p.path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                )
            })
            .collect();
        eprintln!(
            "Warning: gemspec directive in {} found multiple gemspecs in {} ({}) — using {}. Pass `name:` to disambiguate.",
            gemfile_path.display(),
            spec_dir.display(),
            listed.join(", "),
            parsed[0].name
        );
    }

    parsed.into_iter().next()
}

/// Statically parse a `.gemspec` for its name and development dependencies.
fn load_gemspec(spec_path: &Path, spec_dir: &Path) -> Option<ParsedGemspec> {
    let source = match std::fs::read(spec_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "Warning: Could not read gemspec {}: {}",
                spec_path.display(),
                e
            );
            return None;
        }
    };

    let parse_result = ruby_prism::parse(&source);
    let mut visitor = GemspecVisitor {
        source: &source,
        spec_path: spec_path.to_path_buf(),
        receiver_names: HashSet::new(),
        spec_name: None,
        development_deps: Vec::new(),
    };
    visitor.visit(&parse_result.node());

    Some(ParsedGemspec {
        path: spec_path.to_path_buf(),
        name: visitor
            .spec_name
            .unwrap_or_else(|| gemspec_stem(spec_path, spec_dir)),
        development_deps: visitor.development_deps,
    })
}

/// Register the entries Bundler would add for a gemspec: the gem itself (as a path
/// source) plus its development dependencies. Runtime dependencies are resolved
/// transitively by Bundler and are intentionally not top-level gems.
fn register_gemspec(
    ctx: &mut ParseCtx,
    spec: ParsedGemspec,
    spec_dir: &Path,
    self_groups: &[String],
    dev_groups: &[String],
    platforms: &[String],
) {
    debug_assert!(spec.path.is_file());

    ctx.gems.push(PendingGem {
        entry: GemEntry {
            name: spec.name,
            groups: self_groups.to_vec(),
            platforms: platforms.to_vec(),
            require_paths: Vec::new(),
            module_names: Vec::new(),
            require_false: false,
            source: Some(crate::types::GemSource::Path(spec_dir.to_path_buf())),
        },
        gemspec_dev_dep: false,
    });

    for dep in spec.development_deps {
        ctx.gems.push(PendingGem {
            entry: GemEntry {
                name: dep,
                groups: dev_groups.to_vec(),
                platforms: platforms.to_vec(),
                require_paths: Vec::new(),
                module_names: Vec::new(),
                require_false: false,
                source: None,
            },
            gemspec_dev_dep: true,
        });
    }
}

/// Fall back to the gemspec file name (or its directory for a bare `.gemspec`).
fn gemspec_stem(spec_path: &Path, spec_dir: &Path) -> String {
    let file_name = spec_path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default();
    let stem = file_name.trim_end_matches(".gemspec").to_string();
    if !stem.is_empty() {
        return stem;
    }
    spec_dir
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default()
}

struct GemspecVisitor<'s> {
    source: &'s [u8],
    spec_path: PathBuf,
    /// Local variable names bound to a `Gem::Specification` instance.
    receiver_names: HashSet<Vec<u8>>,
    spec_name: Option<String>,
    development_deps: Vec<String>,
}

impl GemspecVisitor<'_> {
    fn is_spec_receiver(&self, call: &ruby_prism::CallNode<'_>) -> bool {
        call.receiver()
            .and_then(|r| r.as_local_variable_read_node())
            .map(|v| self.receiver_names.contains(v.name().as_slice()))
            .unwrap_or(false)
    }
}

/// True for `Gem::Specification.new` (and the rarer bare `Specification.new`).
fn is_gem_specification_new(call: &ruby_prism::CallNode<'_>) -> bool {
    call.name().as_slice() == b"new" && receiver_is_const(call, b"Specification")
}

impl Visit<'_> for GemspecVisitor<'_> {
    fn visit_local_variable_write_node(&mut self, node: &ruby_prism::LocalVariableWriteNode<'_>) {
        // spec = Gem::Specification.new
        if let Some(call) = node.value().as_call_node() {
            if is_gem_specification_new(&call) {
                self.receiver_names.insert(node.name().as_slice().to_vec());
            }
        }
        ruby_prism::visit_local_variable_write_node(self, node);
    }

    fn visit_call_node(&mut self, node: &ruby_prism::CallNode<'_>) {
        // Gem::Specification.new do |spec| ... end
        if is_gem_specification_new(node) {
            if let Some(block) = node.block().and_then(|b| b.as_block_node()) {
                if let Some(params) = block
                    .parameters()
                    .and_then(|p| p.as_block_parameters_node())
                    .and_then(|p| p.parameters())
                {
                    if let Some(first) = params.requireds().iter().next() {
                        if let Some(req) = first.as_required_parameter_node() {
                            self.receiver_names.insert(req.name().as_slice().to_vec());
                        }
                    }
                }
            }
            ruby_prism::visit_call_node(self, node);
            return;
        }

        let name = node.name();
        let name_bytes = name.as_slice();

        if name_bytes == b"name=" && self.is_spec_receiver(node) {
            if let Some(args) = node.arguments() {
                if let Some(first) = args.arguments().iter().next() {
                    match extract_string_value(&first, self.source) {
                        Some(n) => self.spec_name = Some(n),
                        None => eprintln!(
                            "Warning: could not statically resolve spec.name in {} — falling back to the file name",
                            self.spec_path.display()
                        ),
                    }
                }
            }
        } else if matches!(
            name_bytes,
            b"add_dependency" | b"add_runtime_dependency" | b"add_development_dependency"
        ) && self.is_spec_receiver(node)
        {
            let is_dev = name_bytes == b"add_development_dependency";
            if let Some(args) = node.arguments() {
                if let Some(first) = args.arguments().iter().next() {
                    match extract_string_value(&first, self.source)
                        .or_else(|| extract_symbol_value(&first, self.source))
                    {
                        Some(dep) => {
                            // Runtime dependencies belong to the gem itself, not to the
                            // top-level Gemfile, so only development deps are recorded.
                            if is_dev {
                                self.development_deps.push(dep);
                            }
                        }
                        None => eprintln!(
                            "Warning: unsupported dependency declaration in {} — ignored",
                            self.spec_path.display()
                        ),
                    }
                }
            }
        }

        ruby_prism::visit_call_node(self, node);
    }
}

/// Merge duplicate gem declarations by name.
///
/// Bundler always prefers an explicit Gemfile declaration over a duplicate coming
/// from a gemspec's development dependencies (in either declaration order); two
/// Gemfile declarations, or two gemspec development dependencies, are unioned.
/// The position of the first occurrence is preserved.
fn merge_duplicate_gems(gems: Vec<PendingGem>) -> Vec<GemEntry> {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut result: Vec<PendingGem> = Vec::new();

    for pending in gems {
        let idx = match seen.get(&pending.entry.name) {
            Some(&idx) => idx,
            None => {
                seen.insert(pending.entry.name.clone(), result.len());
                result.push(pending);
                continue;
            }
        };

        let existing = &mut result[idx];
        match (existing.gemspec_dev_dep, pending.gemspec_dev_dep) {
            // A later Gemfile declaration replaces the gemspec development dependency.
            (true, false) => {
                *existing = pending;
                continue;
            }
            // An explicit Gemfile declaration already won; drop the gemspec dev dep.
            (false, true) => continue,
            _ => {}
        }

        let gem = pending.entry;
        let existing = &mut existing.entry;
        // groups: set union preserving order
        for g in &gem.groups {
            if !existing.groups.contains(g) {
                existing.groups.push(g.clone());
            }
        }
        // platforms: empty on either side means "all platforms" → result should be empty
        if existing.platforms.is_empty() || gem.platforms.is_empty() {
            existing.platforms.clear();
        } else {
            for p in &gem.platforms {
                if !existing.platforms.contains(p) {
                    existing.platforms.push(p.clone());
                }
            }
        }
        // require_paths: union preserving order
        for rp in &gem.require_paths {
            if !existing.require_paths.contains(rp) {
                existing.require_paths.push(rp.clone());
            }
        }
        // require_false: logical OR
        existing.require_false = existing.require_false || gem.require_false;
        if existing.source.is_none() {
            existing.source = gem.source;
        }
    }

    result.into_iter().map(|p| p.entry).collect()
}

// --- Helper functions for extracting values from Prism AST nodes ---

fn extract_string_value(node: &Node<'_>, _source: &[u8]) -> Option<String> {
    if let Some(s) = node.as_string_node() {
        return Some(String::from_utf8_lossy(s.unescaped()).to_string());
    }
    None
}

fn extract_symbol_value(node: &Node<'_>, _source: &[u8]) -> Option<String> {
    if let Some(s) = node.as_symbol_node() {
        return Some(String::from_utf8_lossy(s.unescaped()).to_string());
    }
    None
}

/// Extract a key name from a hash key node (symbol or string).
fn extract_key_name(node: &Node<'_>, source: &[u8]) -> Option<String> {
    extract_symbol_value(node, source).or_else(|| extract_string_value(node, source))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_gems(gemfile_content: &str) -> Vec<GemEntry> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Gemfile"), gemfile_content).unwrap();
        parse_gemfile(dir.path())
    }

    fn find_gem<'a>(gems: &'a [GemEntry], name: &str) -> &'a GemEntry {
        gems.iter()
            .find(|g| g.name == name)
            .unwrap_or_else(|| panic!("gem '{}' not found", name))
    }

    fn names(gems: &[GemEntry]) -> Vec<&str> {
        gems.iter().map(|g| g.name.as_str()).collect()
    }

    #[test]
    fn basic_gem() {
        let gems = parse_gems("source 'https://rubygems.org'\ngem 'rails'\n");
        assert_eq!(gems.len(), 1);
        assert_eq!(gems[0].name, "rails");
        assert_eq!(gems[0].groups, vec!["default"]);
    }

    #[test]
    fn gem_with_version() {
        let gems = parse_gems("gem 'rails', '~> 7.1'\n");
        assert_eq!(gems[0].name, "rails");
    }

    #[test]
    fn gem_require_false() {
        let gems = parse_gems("gem 'bootsnap', require: false\n");
        assert!(gems[0].require_false);
        assert!(gems[0].require_paths.is_empty());
    }

    #[test]
    fn gem_require_empty_array_disables_autorequire() {
        let gems = parse_gems("gem 'bootsnap', require: []\n");
        assert!(gems[0].require_false);
        assert!(gems[0].require_paths.is_empty());
    }

    #[test]
    fn gem_require_string() {
        let gems = parse_gems("gem 'ruby-kafka', require: 'kafka'\n");
        assert!(!gems[0].require_false);
        assert_eq!(gems[0].require_paths, vec!["kafka"]);
    }

    #[test]
    fn gem_require_array() {
        let gems = parse_gems("gem 'ddtrace', require: ['datadog', 'ddtrace']\n");
        assert_eq!(gems[0].require_paths, vec!["datadog", "ddtrace"]);
    }

    #[test]
    fn gem_in_group() {
        let gems = parse_gems("group :test do\n  gem 'rspec'\nend\n");
        assert_eq!(gems[0].groups, vec!["test"]);
    }

    #[test]
    fn gem_in_nested_groups() {
        let gems = parse_gems("group :development, :test do\n  gem 'pry'\nend\n");
        assert_eq!(gems[0].groups, vec!["development", "test"]);
    }

    #[test]
    fn gem_group_keyword() {
        let gems = parse_gems("gem 'rspec', group: :test\n");
        assert_eq!(gems[0].groups, vec!["test"]);
    }

    #[test]
    fn multiple_gems() {
        let gems = parse_gems("gem 'rails'\ngem 'puma'\ngem 'pg'\n");
        assert_eq!(gems.len(), 3);
        assert_eq!(names(&gems), vec!["rails", "puma", "pg"]);
    }

    #[test]
    fn gem_groups_keyword_array() {
        let gems = parse_gems("gem 'rspec', groups: [:test, :development]\n");
        let g = find_gem(&gems, "rspec");
        assert!(g.groups.contains(&"test".to_string()));
        assert!(g.groups.contains(&"development".to_string()));
    }

    #[test]
    fn gem_require_array_of_paths() {
        let gems = parse_gems(
            "gem 'ddtrace', require: ['datadog/profiling', 'ddtrace/auto_instrument']\n",
        );
        let g = find_gem(&gems, "ddtrace");
        assert_eq!(g.require_paths.len(), 2);
    }

    #[test]
    fn gem_path_source_is_resolved_from_declaring_gemfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("engines/admin")).unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "gem 'admin_tools', path: 'engines/admin'\n",
        )
        .unwrap();
        let gems = parse_gemfile(dir.path());
        let g = find_gem(&gems, "admin_tools");
        assert_eq!(
            g.source.as_ref(),
            Some(&crate::types::GemSource::Path(
                dir.path().join("engines/admin")
            ))
        );
    }

    #[test]
    fn gem_git_source_is_retained() {
        let gems = parse_gems("gem 'custom_gem', git: 'https://example.test/custom.git'\n");
        let g = find_gem(&gems, "custom_gem");
        assert_eq!(
            g.source.as_ref(),
            Some(&crate::types::GemSource::Git(
                "https://example.test/custom.git".to_string()
            ))
        );
    }

    #[test]
    fn parse_gemfiles_merges_multiple_explicit_gemfiles() {
        let dir = tempfile::tempdir().unwrap();
        let engine = dir.path().join("engines/admin");
        std::fs::create_dir_all(&engine).unwrap();
        let root_gemfile = dir.path().join("Gemfile");
        let engine_gemfile = engine.join("Gemfile");
        std::fs::write(&root_gemfile, "gem 'rails'\n").unwrap();
        std::fs::write(&engine_gemfile, "gem 'puma'\n").unwrap();
        let gems = parse_gemfiles(dir.path(), &[root_gemfile, engine_gemfile]);
        assert_eq!(names(&gems), vec!["rails", "puma"]);
    }

    #[test]
    fn parses_platforms_key_single() {
        let gems = parse_gems("gem 'x', platforms: :mingw\n");
        let g = find_gem(&gems, "x");
        assert_eq!(g.platforms, vec!["mingw"]);
    }

    #[test]
    fn parses_platforms_key_array() {
        let gems = parse_gems("gem 'x', platforms: [:mingw, :x64_mingw]\n");
        let g = find_gem(&gems, "x");
        assert_eq!(g.platforms, vec!["mingw", "x64_mingw"]);
    }

    #[test]
    fn parses_platform_singular_key() {
        let gems = parse_gems("gem 'x', platform: :jruby\n");
        let g = find_gem(&gems, "x");
        assert_eq!(g.platforms, vec!["jruby"]);
    }

    #[test]
    fn parses_platforms_block() {
        let gems = parse_gems("platforms :mingw do\n  gem 'x'\nend\n");
        let g = find_gem(&gems, "x");
        assert_eq!(g.platforms, vec!["mingw"]);
    }

    #[test]
    fn merges_nested_platforms_and_groups() {
        let gems =
            parse_gems("platforms :mingw do\n  group :development do\n    gem 'x'\n  end\nend\n");
        let g = find_gem(&gems, "x");
        assert_eq!(g.platforms, vec!["mingw"]);
        assert!(g.groups.contains(&"development".to_string()));
    }

    #[test]
    fn duplicate_gems_merge_groups() {
        let gems = parse_gems("gem 'x', group: :dev\ngem 'x', group: :test\n");
        assert_eq!(gems.iter().filter(|g| g.name == "x").count(), 1);
        let g = find_gem(&gems, "x");
        assert!(g.groups.contains(&"dev".to_string()));
        assert!(g.groups.contains(&"test".to_string()));
    }

    #[test]
    fn duplicate_gems_merge_platforms() {
        let gems = parse_gems("gem 'x', platforms: :mingw\ngem 'x'\n");
        assert_eq!(gems.iter().filter(|g| g.name == "x").count(), 1);
        let g = find_gem(&gems, "x");
        // No platforms on second line means "all platforms" → merged result is empty (all)
        assert!(g.platforms.is_empty());
    }

    #[test]
    fn duplicate_gems_require_false_union() {
        let gems = parse_gems("gem 'x', require: false\ngem 'x', require: 'x/init'\n");
        assert_eq!(gems.iter().filter(|g| g.name == "x").count(), 1);
        let g = find_gem(&gems, "x");
        assert!(g.require_false);
        assert!(g.require_paths.contains(&"x/init".to_string()));
    }

    #[test]
    fn duplicate_gems_preserve_source_metadata() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("vendor/x")).unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "gem 'x'\ngem 'x', path: 'vendor/x'\n",
        )
        .unwrap();
        let gems = parse_gemfile(dir.path());
        let g = find_gem(&gems, "x");
        assert_eq!(
            g.source.as_ref(),
            Some(&crate::types::GemSource::Path(dir.path().join("vendor/x")))
        );
    }

    // ── eval_gemfile ──

    #[test]
    fn eval_gemfile_relative_string() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "eval_gemfile('Gemfile.shared')\ngem 'rails'\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("Gemfile.shared"), "gem 'puma'\n").unwrap();
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["puma", "rails"]);
    }

    #[test]
    fn eval_gemfile_preserves_source_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "gem 'a'\neval_gemfile 'Gemfile.shared'\ngem 'c'\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("Gemfile.shared"), "gem 'b'\n").unwrap();
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["a", "b", "c"]);
    }

    #[test]
    fn eval_gemfile_file_expand_path_with_file_constant() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "eval_gemfile File.expand_path(\"../shared/Gemfile\", __FILE__)\ngem 'rails'\n",
        )
        .unwrap();
        std::fs::write(shared.join("Gemfile"), "gem 'puma'\n").unwrap();
        let gems = parse_gemfile(dir.path());
        assert!(names(&gems).contains(&"puma"));
        assert!(names(&gems).contains(&"rails"));
    }

    #[test]
    fn eval_gemfile_file_expand_path_treats_file_constant_as_directory() {
        // Ruby's File.expand_path("shared/Gemfile", __FILE__) resolves *below* the
        // Gemfile path itself, so this must NOT pick up ./shared/Gemfile.
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "eval_gemfile File.expand_path(\"shared/Gemfile\", __FILE__)\ngem 'rails'\n",
        )
        .unwrap();
        std::fs::write(shared.join("Gemfile"), "gem 'puma'\n").unwrap();
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["rails"]);
    }

    #[test]
    fn eval_gemfile_file_expand_path_with_dir_method() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "eval_gemfile File.expand_path(\"shared/Gemfile\", __dir__)\n",
        )
        .unwrap();
        std::fs::write(shared.join("Gemfile"), "gem 'puma'\n").unwrap();
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["puma"]);
    }

    #[test]
    fn eval_gemfile_file_join_with_dirname() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "eval_gemfile File.join(File.dirname(__FILE__), 'shared', 'Gemfile')\n",
        )
        .unwrap();
        std::fs::write(shared.join("Gemfile"), "gem 'puma'\n").unwrap();
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["puma"]);
    }

    #[test]
    fn nested_eval_gemfile_resolves_against_containing_gemfile() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("gemfiles/shared");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.path().join("Gemfile"), "eval_gemfile 'gemfiles/base'\n").unwrap();
        std::fs::write(
            dir.path().join("gemfiles/base"),
            "gem 'base_gem'\neval_gemfile 'shared/extra'\n",
        )
        .unwrap();
        std::fs::write(sub.join("extra"), "gem 'extra_gem'\n").unwrap();
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["base_gem", "extra_gem"]);
    }

    #[test]
    fn nested_eval_gemfile_resolves_parent_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("gemfiles");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.path().join("Gemfile"), "eval_gemfile 'gemfiles/base'\n").unwrap();
        std::fs::write(sub.join("base"), "eval_gemfile '../Gemfile.common'\n").unwrap();
        std::fs::write(dir.path().join("Gemfile.common"), "gem 'common_gem'\n").unwrap();
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["common_gem"]);
    }

    #[test]
    fn eval_gemfile_preserves_enclosing_group_context() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "group :test do\n  eval_gemfile 'Gemfile.test'\nend\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("Gemfile.test"),
            "gem 'rspec'\ngroup :ci do\n  gem 'simplecov'\nend\n",
        )
        .unwrap();
        let gems = parse_gemfile(dir.path());
        assert_eq!(find_gem(&gems, "rspec").groups, vec!["test"]);
        assert_eq!(find_gem(&gems, "simplecov").groups, vec!["test", "ci"]);
    }

    #[test]
    fn eval_gemfile_preserves_enclosing_platform_context() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "platforms :jruby do\n  eval_gemfile 'Gemfile.jruby'\nend\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("Gemfile.jruby"), "gem 'jdbc-postgres'\n").unwrap();
        let gems = parse_gemfile(dir.path());
        assert_eq!(find_gem(&gems, "jdbc-postgres").platforms, vec!["jruby"]);
    }

    #[test]
    fn eval_gemfile_cycles_terminate() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "gem 'rails'\neval_gemfile 'Gemfile.a'\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("Gemfile.a"),
            "gem 'a_gem'\neval_gemfile 'Gemfile.b'\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("Gemfile.b"),
            "gem 'b_gem'\neval_gemfile 'Gemfile.a'\n",
        )
        .unwrap();
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["rails", "a_gem", "b_gem"]);
    }

    #[test]
    fn eval_gemfile_self_reference_terminates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "eval_gemfile './Gemfile'\ngem 'rails'\n",
        )
        .unwrap();
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["rails"]);
    }

    #[test]
    fn eval_gemfile_path_aliases_merge_into_one_entry() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "eval_gemfile 'shared/Gemfile'\neval_gemfile './shared/../shared/Gemfile'\n",
        )
        .unwrap();
        std::fs::write(shared.join("Gemfile"), "gem 'puma'\n").unwrap();
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["puma"]);
    }

    #[cfg(unix)]
    #[test]
    fn eval_gemfile_symlink_aliases_merge_into_one_entry() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("Gemfile"), "gem 'puma'\n").unwrap();
        std::os::unix::fs::symlink(&real, dir.path().join("link")).unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "eval_gemfile 'real/Gemfile'\neval_gemfile 'link/Gemfile'\n",
        )
        .unwrap();
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["puma"]);
    }

    #[test]
    fn repeated_eval_gemfile_merges_each_group_context() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "eval_gemfile 'shared.gemfile'\ngroup :test do\n  eval_gemfile 'shared.gemfile'\nend\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("shared.gemfile"), "gem 'puma'\n").unwrap();
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["puma"]);
        // The second evaluation must still contribute its enclosing group.
        assert_eq!(find_gem(&gems, "puma").groups, vec!["default", "test"]);
    }

    #[test]
    fn repeated_eval_gemfile_merges_each_platform_context() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "platforms :jruby do\n  eval_gemfile 'shared.gemfile'\nend\nplatforms :mingw do\n  eval_gemfile 'shared.gemfile'\nend\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("shared.gemfile"), "gem 'puma'\n").unwrap();
        let gems = parse_gemfile(dir.path());
        assert_eq!(find_gem(&gems, "puma").platforms, vec!["jruby", "mingw"]);
    }

    #[test]
    fn repeated_eval_gemfile_via_aliases_merges_contexts() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "eval_gemfile 'shared/Gemfile'\ngroup :test do\n  eval_gemfile './shared/../shared/Gemfile'\nend\n",
        )
        .unwrap();
        std::fs::write(shared.join("Gemfile"), "gem 'puma'\n").unwrap();
        let gems = parse_gemfile(dir.path());
        assert_eq!(find_gem(&gems, "puma").groups, vec!["default", "test"]);
    }

    #[test]
    fn eval_gemfile_nonexistent_is_warned_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "eval_gemfile('nonexistent')\ngem 'rails'\n",
        )
        .unwrap();
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["rails"]);
    }

    #[test]
    fn eval_gemfile_unsupported_expression_is_warned_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "eval_gemfile Bundler.root.join('Gemfile.shared')\ngem 'rails'\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("Gemfile.shared"), "gem 'puma'\n").unwrap();
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["rails"]);
    }

    #[test]
    fn eval_gemfile_without_arguments_is_warned_not_fatal() {
        let gems = parse_gems("eval_gemfile\ngem 'rails'\n");
        assert_eq!(names(&gems), vec!["rails"]);
    }

    #[test]
    fn ruby_file_join_matches_ruby_semantics() {
        // Ruby: File.join('a', '/b', 'c') == "a/b/c"
        assert_eq!(
            ruby_file_join(&["a".into(), "/b".into(), "c".into()]),
            "a/b/c"
        );
        assert_eq!(ruby_file_join(&["/a".into(), "b".into()]), "/a/b");
        assert_eq!(ruby_file_join(&["a/".into(), "/b".into()]), "a/b");
        assert_eq!(ruby_file_join(&["/".into(), "a".into()]), "/a");
        assert_eq!(ruby_file_join(&["a".into(), "".into()]), "a/");
        assert_eq!(ruby_file_join(&["a".into()]), "a");
        assert_eq!(
            ruby_file_join(&["a//".into(), "//b".into(), "c".into()]),
            "a/b/c"
        );
    }

    #[test]
    fn eval_gemfile_file_join_does_not_restart_on_absolute_component() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "eval_gemfile File.join(__dir__, '/sub', 'Gemfile')\n",
        )
        .unwrap();
        std::fs::write(sub.join("Gemfile"), "gem 'puma'\n").unwrap();
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["puma"]);
    }

    #[test]
    fn lexical_normalize_resolves_dot_segments() {
        assert_eq!(
            lexical_normalize(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
        assert_eq!(lexical_normalize(Path::new("/..")), PathBuf::from("/"));
        assert_eq!(lexical_normalize(Path::new("a/../..")), PathBuf::from(".."));
        assert_eq!(lexical_normalize(Path::new("./")), PathBuf::from("."));
    }

    // ── gemspec ──

    fn write_gemspec(path: &Path, body: &str) {
        std::fs::write(
            path,
            format!("Gem::Specification.new do |spec|\n{}\nend\n", body),
        )
        .unwrap();
    }

    #[test]
    fn gemspec_adds_self_as_default_group_path_source() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Gemfile"), "gemspec\n").unwrap();
        write_gemspec(&dir.path().join("foo.gemspec"), "  spec.name = 'my_gem'\n");
        let gems = parse_gemfile(dir.path());
        let g = find_gem(&gems, "my_gem");
        assert_eq!(g.groups, vec!["default"]);
        assert_eq!(
            g.source.as_ref(),
            Some(&crate::types::GemSource::Path(dir.path().to_path_buf()))
        );
    }

    #[test]
    fn gemspec_runtime_dependencies_are_not_top_level_gems() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Gemfile"), "gemspec\n").unwrap();
        write_gemspec(
            &dir.path().join("foo.gemspec"),
            "  spec.name = 'my_gem'\n  spec.add_dependency 'faraday'\n  spec.add_runtime_dependency 'activesupport'\n  spec.add_development_dependency 'rspec'\n",
        );
        let gems = parse_gemfile(dir.path());
        assert!(!gems.iter().any(|g| g.name == "faraday"));
        assert!(!gems.iter().any(|g| g.name == "activesupport"));
        assert_eq!(find_gem(&gems, "rspec").groups, vec!["development"]);
        assert_eq!(names(&gems), vec!["my_gem", "rspec"]);
    }

    #[test]
    fn gemspec_honors_development_group_option() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "gemspec development_group: :dev\n",
        )
        .unwrap();
        write_gemspec(
            &dir.path().join("foo.gemspec"),
            "  spec.name = 'my_gem'\n  spec.add_development_dependency 'rubocop'\n",
        );
        let gems = parse_gemfile(dir.path());
        assert_eq!(find_gem(&gems, "rubocop").groups, vec!["dev"]);
    }

    #[test]
    fn gemspec_honors_path_option() {
        let dir = tempfile::tempdir().unwrap();
        let engine = dir.path().join("engines/auth");
        std::fs::create_dir_all(&engine).unwrap();
        std::fs::write(dir.path().join("Gemfile"), "gemspec path: 'engines/auth'\n").unwrap();
        write_gemspec(
            &engine.join("auth.gemspec"),
            "  spec.name = 'auth'\n  spec.add_development_dependency 'factory_bot'\n",
        );
        let gems = parse_gemfile(dir.path());
        let g = find_gem(&gems, "auth");
        assert_eq!(
            g.source.as_ref(),
            Some(&crate::types::GemSource::Path(engine.clone()))
        );
        assert_eq!(find_gem(&gems, "factory_bot").groups, vec!["development"]);
    }

    #[test]
    fn gemspec_honors_name_option_to_disambiguate() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Gemfile"), "gemspec name: 'beta'\n").unwrap();
        write_gemspec(
            &dir.path().join("alpha.gemspec"),
            "  spec.name = 'alpha'\n  spec.add_development_dependency 'alpha_dev'\n",
        );
        write_gemspec(
            &dir.path().join("beta.gemspec"),
            "  spec.name = 'beta'\n  spec.add_development_dependency 'beta_dev'\n",
        );
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["beta", "beta_dev"]);
    }

    #[test]
    fn gemspec_name_option_matches_parsed_spec_name_not_file_name() {
        // Bundler filters candidates on the parsed spec.name, so a `project.gemspec`
        // declaring `foo` must satisfy `gemspec name: 'foo'`.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Gemfile"), "gemspec name: 'foo'\n").unwrap();
        write_gemspec(
            &dir.path().join("project.gemspec"),
            "  spec.name = 'foo'\n  spec.add_development_dependency 'rspec'\n",
        );
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["foo", "rspec"]);
    }

    #[test]
    fn gemspec_name_option_ignores_file_name_only_match() {
        // `foo.gemspec` that actually declares `other` must not satisfy `name: 'foo'`.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "gemspec name: 'foo'\ngem 'rails'\n",
        )
        .unwrap();
        write_gemspec(&dir.path().join("foo.gemspec"), "  spec.name = 'other'\n");
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["rails"]);
    }

    #[test]
    fn gemspec_same_name_in_multiple_files_is_not_ambiguous() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Gemfile"), "gemspec\n").unwrap();
        write_gemspec(
            &dir.path().join("a.gemspec"),
            "  spec.name = 'same'\n  spec.add_development_dependency 'rspec'\n",
        );
        write_gemspec(&dir.path().join("b.gemspec"), "  spec.name = 'same'\n");
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["same", "rspec"]);
    }

    #[test]
    fn gemspec_name_option_without_match_is_warned_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "gemspec name: 'missing'\ngem 'rails'\n",
        )
        .unwrap();
        write_gemspec(&dir.path().join("alpha.gemspec"), "  spec.name = 'alpha'\n");
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["rails"]);
    }

    #[test]
    fn gemspec_multiple_candidates_selected_deterministically() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Gemfile"), "gemspec\n").unwrap();
        write_gemspec(&dir.path().join("zeta.gemspec"), "  spec.name = 'zeta'\n");
        write_gemspec(&dir.path().join("alpha.gemspec"), "  spec.name = 'alpha'\n");
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["alpha"]);
    }

    #[test]
    fn gemspec_no_candidates_is_warned_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Gemfile"), "gemspec\ngem 'rails'\n").unwrap();
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["rails"]);
    }

    #[test]
    fn gemspec_discovers_bare_dot_gemspec_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Gemfile"), "gemspec\n").unwrap();
        write_gemspec(&dir.path().join(".gemspec"), "  spec.name = 'dotted'\n");
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["dotted"]);
    }

    #[test]
    fn gemspec_ignores_hidden_and_nested_gemspecs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("nested")).unwrap();
        std::fs::write(dir.path().join("Gemfile"), "gemspec\n").unwrap();
        write_gemspec(&dir.path().join("real.gemspec"), "  spec.name = 'real'\n");
        write_gemspec(
            &dir.path().join(".hidden.gemspec"),
            "  spec.name = 'hidden'\n",
        );
        write_gemspec(
            &dir.path().join("nested/nested.gemspec"),
            "  spec.name = 'nested'\n",
        );
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["real"]);
    }

    #[test]
    fn gemspec_dependency_calls_require_specification_receiver() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Gemfile"), "gemspec\n").unwrap();
        std::fs::write(
            dir.path().join("foo.gemspec"),
            r#"
other = Object.new
other.add_development_dependency 'not_a_dep'
add_development_dependency 'also_not_a_dep'
Gem::Specification.new do |spec|
  spec.name = 'my_gem'
  spec.add_development_dependency 'real_dev_dep'
end
"#,
        )
        .unwrap();
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["my_gem", "real_dev_dep"]);
    }

    #[test]
    fn gemspec_supports_assigned_specification_variable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Gemfile"), "gemspec\n").unwrap();
        std::fs::write(
            dir.path().join("foo.gemspec"),
            r#"
spec = Gem::Specification.new
spec.name = 'assigned_gem'
spec.add_development_dependency 'rspec'
"#,
        )
        .unwrap();
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["assigned_gem", "rspec"]);
    }

    #[test]
    fn gemspec_falls_back_to_file_name_when_spec_name_is_dynamic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Gemfile"), "gemspec\n").unwrap();
        write_gemspec(
            &dir.path().join("fallback.gemspec"),
            "  spec.name = File.basename(__FILE__, '.gemspec')\n",
        );
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["fallback"]);
    }

    #[test]
    fn gemspec_inside_group_block_inherits_group() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "group :engine do\n  gemspec\nend\n",
        )
        .unwrap();
        write_gemspec(
            &dir.path().join("foo.gemspec"),
            "  spec.name = 'my_gem'\n  spec.add_development_dependency 'rspec'\n",
        );
        let gems = parse_gemfile(dir.path());
        assert_eq!(find_gem(&gems, "my_gem").groups, vec!["engine"]);
        assert_eq!(
            find_gem(&gems, "rspec").groups,
            vec!["engine", "development"]
        );
    }

    #[test]
    fn gemfile_declaration_wins_over_gemspec_dev_dep_when_declared_first() {
        // Bundler: "Always prefer the dependency from the Gemfile".
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "gem 'rspec', group: :test\ngemspec\n",
        )
        .unwrap();
        write_gemspec(
            &dir.path().join("foo.gemspec"),
            "  spec.name = 'my_gem'\n  spec.add_development_dependency 'rspec'\n",
        );
        let gems = parse_gemfile(dir.path());
        assert_eq!(gems.iter().filter(|g| g.name == "rspec").count(), 1);
        assert_eq!(find_gem(&gems, "rspec").groups, vec!["test"]);
    }

    #[test]
    fn gemfile_declaration_wins_over_gemspec_dev_dep_when_declared_last() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "gemspec\ngem 'rspec', group: :test, require: false\n",
        )
        .unwrap();
        write_gemspec(
            &dir.path().join("foo.gemspec"),
            "  spec.name = 'my_gem'\n  spec.add_development_dependency 'rspec'\n",
        );
        let gems = parse_gemfile(dir.path());
        assert_eq!(gems.iter().filter(|g| g.name == "rspec").count(), 1);
        let rspec = find_gem(&gems, "rspec");
        assert_eq!(rspec.groups, vec!["test"]);
        assert!(rspec.require_false);
    }

    #[test]
    fn duplicate_gemspec_dev_deps_still_union_groups() {
        let dir = tempfile::tempdir().unwrap();
        let engine = dir.path().join("engine");
        std::fs::create_dir_all(&engine).unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "gemspec\ngemspec path: 'engine', development_group: :dev\n",
        )
        .unwrap();
        write_gemspec(
            &dir.path().join("foo.gemspec"),
            "  spec.name = 'my_gem'\n  spec.add_development_dependency 'rspec'\n",
        );
        write_gemspec(
            &engine.join("engine.gemspec"),
            "  spec.name = 'engine'\n  spec.add_development_dependency 'rspec'\n",
        );
        let gems = parse_gemfile(dir.path());
        assert_eq!(gems.iter().filter(|g| g.name == "rspec").count(), 1);
        assert_eq!(find_gem(&gems, "rspec").groups, vec!["development", "dev"]);
    }

    #[test]
    fn root_gemspec_not_parsed_without_directive() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Gemfile"), "gem 'rails'\n").unwrap();
        write_gemspec(
            &dir.path().join("stale.gemspec"),
            "  spec.name = 'phantom'\n  spec.add_development_dependency 'phantom_dep'\n",
        );
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["rails"]);
    }

    #[test]
    fn parse_gemfiles_parses_bare_gemspec_next_to_explicit_gemfile() {
        let dir = tempfile::tempdir().unwrap();
        let engine = dir.path().join("engines/admin");
        std::fs::create_dir_all(&engine).unwrap();
        let engine_gemfile = engine.join("Gemfile");
        std::fs::write(&engine_gemfile, "gemspec\n").unwrap();
        write_gemspec(
            &engine.join("admin.gemspec"),
            "  spec.name = 'admin'\n  spec.add_development_dependency 'devise'\n",
        );
        let gems = parse_gemfiles(dir.path(), &[engine_gemfile]);
        assert_eq!(names(&gems), vec!["admin", "devise"]);
    }

    #[test]
    fn gemspec_reached_through_eval_gemfile_resolves_relative_to_that_gemfile() {
        let dir = tempfile::tempdir().unwrap();
        let engine = dir.path().join("engines/core");
        std::fs::create_dir_all(&engine).unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "eval_gemfile 'engines/core/Gemfile'\n",
        )
        .unwrap();
        std::fs::write(engine.join("Gemfile"), "gemspec\n").unwrap();
        write_gemspec(
            &engine.join("core.gemspec"),
            "  spec.name = 'core'\n  spec.add_development_dependency 'rspec'\n",
        );
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["core", "rspec"]);
        assert_eq!(
            find_gem(&gems, "core").source.as_ref(),
            Some(&crate::types::GemSource::Path(engine))
        );
    }
}
