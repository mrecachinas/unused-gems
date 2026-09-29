use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use ruby_prism::{self, Node, Visit};

use crate::types::{GemDeclaration, GemEntry, GemSource, InstallCondition};

const DEFAULT_DEVELOPMENT_GROUP: &str = "development";

/// Bundler's default Gemfile discovery for `dir`: `Gemfile`, then `gems.rb`.
/// Returns `<dir>/Gemfile` when neither exists so callers report that name.
pub fn default_gemfile_path(dir: &Path) -> PathBuf {
    let gemfile = dir.join("Gemfile");
    if gemfile.is_file() {
        return gemfile;
    }
    let gems_rb = dir.join("gems.rb");
    if gems_rb.is_file() {
        return gems_rb;
    }
    gemfile
}

/// Parse a Gemfile using ruby-prism AST and extract gem declarations.
pub fn parse_gemfile(dir: &Path) -> Vec<GemEntry> {
    let gemfile_path = default_gemfile_path(dir);
    if !gemfile_path.exists() {
        eprintln!("Error: No Gemfile or gems.rb found in {}", dir.display());
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
        git_sources: DEFAULT_GIT_SOURCES.iter().map(|s| s.to_string()).collect(),
    };

    for path in gemfile_paths {
        let abs = if path.is_absolute() {
            path.clone()
        } else {
            dir.join(path)
        };
        // Each top-level Gemfile is its own Bundler::Dsl with default git sources.
        ctx.git_sources = DEFAULT_GIT_SOURCES.iter().map(|s| s.to_string()).collect();
        eval_gemfile_file(&mut ctx, &abs, &DslScope::default());
    }

    merge_duplicate_gems(ctx.gems)
}

/// Upper bound on evaluated Gemfiles, guarding against pathological
/// `eval_gemfile` graphs (a deep diamond can otherwise expand exponentially).
const MAX_GEMFILE_EVALUATIONS: usize = 1024;

/// Git source shorthands Bundler registers by default (`Dsl#add_git_sources`).
const DEFAULT_GIT_SOURCES: &[&str] = &["github", "gist", "bitbucket", "gitlab"];

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
    /// Option keys that name a git source (`github:`, custom `git_source(:name)`).
    git_sources: HashSet<String>,
}

/// A gem declaration plus the provenance Bundler needs to resolve duplicates.
struct PendingGem {
    entry: GemEntry,
    /// True when the entry came from a gemspec's `add_development_dependency`.
    gemspec_dev_dep: bool,
}

/// Block context enclosing a declaration (`group`, `platforms`, `install_if`,
/// `path`/`git`/`github`/`source` blocks). `eval_gemfile` inherits it.
#[derive(Debug, Clone, Default)]
struct DslScope {
    groups: Vec<String>,
    platforms: Vec<String>,
    install_if: Vec<InstallCondition>,
    source: Option<ScopeSource>,
}

#[derive(Debug, Clone)]
enum ScopeSource {
    /// `path "dir" do` — each gem resolves to `dir` or `dir/<subdir>`.
    Path(PathBuf),
    Git(String),
}

fn eval_gemfile_file(ctx: &mut ParseCtx, path: &Path, scope: &DslScope) {
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
        scope: scope.clone(),
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
    scope: DslScope,
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

        let mut autorequire = true;
        let mut require_paths: Vec<String> = Vec::new();
        let mut group_opt: Option<Vec<String>> = None;
        let mut groups_opt: Option<Vec<String>> = None;
        let mut platform_opt: Option<Vec<String>> = None;
        let mut platforms_opt: Option<Vec<String>> = None;
        let mut install_if_opt: Option<InstallCondition> = None;
        let mut source = None;

        // Like Bundler (`args.last.is_a?(Hash)`), only a trailing hash holds options,
        // whether written as keywords or as a braced `{ ... }` literal.
        let options = arg_list[1..].last().and_then(|last| {
            last.as_keyword_hash_node()
                .map(|h| h.elements())
                .or_else(|| last.as_hash_node().map(|h| h.elements()))
        });
        for elem in options.iter().flat_map(|elements| elements.iter()) {
            let assoc = match elem.as_assoc_node() {
                Some(a) => a,
                None => continue,
            };
            let value = assoc.value();
            let key_name = extract_key_name(&assoc.key(), self.source);
            match key_name.as_deref() {
                Some("require") => {
                    // Bundler: `Array(opts["require"] || [])` — false/nil/[] disable
                    // autorequire; `true` means the gem name.
                    if value.as_false_node().is_some() || value.as_nil_node().is_some() {
                        autorequire = false;
                    } else if let Some(s) = extract_string_value(&value, self.source) {
                        require_paths.push(s);
                    } else if let Some(arr) = value.as_array_node() {
                        if arr.elements().is_empty() {
                            autorequire = false;
                        }
                        for el in arr.elements().iter() {
                            if let Some(s) = extract_string_value(&el, self.source) {
                                require_paths.push(s);
                            }
                        }
                    }
                }
                Some("group") => group_opt = Some(extract_name_list(&value, self.source)),
                Some("groups") => groups_opt = Some(extract_name_list(&value, self.source)),
                Some("platform") => platform_opt = Some(extract_name_list(&value, self.source)),
                Some("platforms") => platforms_opt = Some(extract_name_list(&value, self.source)),
                Some("install_if") => install_if_opt = Some(eval_install_condition(&value)),
                Some("path") => {
                    if let Some(s) = extract_string_value(&value, self.source) {
                        let dir = expand_source_path(&s, &self.base_dir);
                        source = Some(GemSource::Path(resolve_path_gem_dir(&dir, &name)));
                    }
                }
                Some("git") => {
                    if let Some(s) = extract_string_value(&value, self.source) {
                        source = Some(GemSource::Git(s));
                    }
                }
                Some(key) if self.ctx.git_sources.contains(key) => {
                    if let Some(s) = extract_string_value(&value, self.source) {
                        source = Some(GemSource::Git(git_source_uri(key, &s)));
                    }
                }
                _ => {}
            }
        }

        // Bundler: `opts["groups"] || opts["group"]` and `opts["platform"] || opts["platforms"]`.
        let mut groups = self.scope.groups.clone();
        groups.extend(groups_opt.or(group_opt).unwrap_or_default());
        if groups.is_empty() {
            groups.push("default".to_string());
        }
        let mut platforms = self.scope.platforms.clone();
        platforms.extend(platform_opt.or(platforms_opt).unwrap_or_default());
        let mut conditions = self.scope.install_if.clone();
        conditions.extend(install_if_opt);

        if source.is_none() {
            source = match &self.scope.source {
                Some(ScopeSource::Path(dir)) => {
                    Some(GemSource::Path(resolve_path_gem_dir(dir, &name)))
                }
                Some(ScopeSource::Git(uri)) => Some(GemSource::Git(uri.clone())),
                None => None,
            };
        }

        let declaration = GemDeclaration {
            groups: groups.clone(),
            platforms: platforms.clone(),
            autorequire,
            install_if: InstallCondition::all(&conditions),
            file: Some(self.gemfile_path.clone()),
            line: Some(line_of(self.source, node.location().start_offset())),
        };

        self.ctx.gems.push(PendingGem {
            entry: GemEntry {
                name,
                groups,
                platforms,
                require_paths,
                module_names: Vec::new(),
                require_false: !autorequire,
                source,
                declarations: vec![declaration],
            },
            gemspec_dev_dep: false,
        });
    }

    /// Positional symbol/string arguments of a block DSL call (`group "test", :ci do`).
    fn extract_block_names(&self, node: &ruby_prism::CallNode<'_>) -> Vec<String> {
        let mut names = Vec::new();
        if let Some(args) = node.arguments() {
            for arg in args.arguments().iter() {
                if let Some(name) = extract_name(&arg, self.source) {
                    names.push(name);
                }
            }
        }
        names
    }

    fn first_string_arg(&self, node: &ruby_prism::CallNode<'_>) -> Option<String> {
        let first = node.arguments()?.arguments().iter().next()?;
        extract_string_value(&first, self.source)
    }

    /// Visit a block-taking DSL call with `scope` active, restoring the outer scope after.
    fn visit_with_scope(&mut self, node: &ruby_prism::CallNode<'_>, scope: DslScope) {
        let outer = std::mem::replace(&mut self.scope, scope);
        ruby_prism::visit_call_node(self, node);
        self.scope = outer;
    }

    /// Evaluate `eval_gemfile <expr>` inline, preserving the enclosing block context.
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

        let scope = self.scope.clone();
        eval_gemfile_file(self.ctx, &target, &scope);
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

        // Bundler >= 2.6 adds the gemspec's own gem via `add_dependency`, bypassing the
        // enclosing `group`/`platforms`/`install_if` blocks: it is always [:default] on
        // all platforms, and development dependencies get only `development_group`.
        let line = line_of(self.source, node.location().start_offset());
        register_gemspec(
            self.ctx,
            spec,
            &spec_dir,
            &development_group,
            &self.gemfile_path,
            line,
        );
    }
}

impl Visit<'_> for GemfileVisitor<'_, '_> {
    fn visit_call_node(&mut self, node: &ruby_prism::CallNode<'_>) {
        if node.receiver().is_some() {
            ruby_prism::visit_call_node(self, node);
            return;
        }
        let name = node.name();
        let has_block = node.block().and_then(|b| b.as_block_node()).is_some();

        match name.as_slice() {
            b"gem" => self.handle_gem(node),
            b"group" => {
                let mut scope = self.scope.clone();
                scope.groups.extend(self.extract_block_names(node));
                return self.visit_with_scope(node, scope);
            }
            b"platforms" | b"platform" => {
                let mut scope = self.scope.clone();
                scope.platforms.extend(self.extract_block_names(node));
                return self.visit_with_scope(node, scope);
            }
            b"install_if" => {
                let mut scope = self.scope.clone();
                if let Some(args) = node.arguments() {
                    scope
                        .install_if
                        .extend(args.arguments().iter().map(|a| eval_install_condition(&a)));
                }
                return self.visit_with_scope(node, scope);
            }
            b"path" if has_block => {
                if let Some(dir) = self.first_string_arg(node) {
                    let mut scope = self.scope.clone();
                    scope.source =
                        Some(ScopeSource::Path(expand_source_path(&dir, &self.base_dir)));
                    return self.visit_with_scope(node, scope);
                }
            }
            b"git" if has_block => {
                if let Some(uri) = self.first_string_arg(node) {
                    let mut scope = self.scope.clone();
                    scope.source = Some(ScopeSource::Git(uri));
                    return self.visit_with_scope(node, scope);
                }
            }
            b"github" if has_block => {
                if let Some(repo) = self.first_string_arg(node) {
                    let mut scope = self.scope.clone();
                    scope.source = Some(ScopeSource::Git(git_source_uri("github", &repo)));
                    return self.visit_with_scope(node, scope);
                }
            }
            b"source" if has_block => {
                // A rubygems source block overrides any enclosing path/git source.
                let mut scope = self.scope.clone();
                scope.source = None;
                return self.visit_with_scope(node, scope);
            }
            b"git_source" => {
                for source_name in self.extract_block_names(node) {
                    self.ctx.git_sources.insert(source_name);
                }
            }
            b"eval_gemfile" => self.handle_eval_gemfile(node),
            b"gemspec" => self.handle_gemspec(node),
            _ => {}
        }

        ruby_prism::visit_call_node(self, node);
    }
}

/// 1-based line number of a byte offset.
fn line_of(source: &[u8], offset: usize) -> usize {
    memchr::memchr_iter(b'\n', &source[..offset.min(source.len())]).count() + 1
}

/// Expand a `path:` / `path "..." do` value like `File.expand_path(path, gemfile_root)`,
/// including a leading `~`.
fn expand_source_path(value: &str, base_dir: &Path) -> PathBuf {
    let home_relative = if value == "~" {
        Some("")
    } else {
        value.strip_prefix("~/")
    };
    let path = match (home_relative, std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(value),
    };
    let path = if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    };
    lexical_normalize(&path)
}

/// Locate a gem inside a path source. Bundler globs `{,*,*/*}.gemspec` below the
/// source directory and picks the spec with the gem's name, so a gem may live in
/// `dir` itself or in an immediate subdirectory (monorepo-style `path` blocks).
fn resolve_path_gem_dir(dir: &Path, name: &str) -> PathBuf {
    let spec_file = format!("{name}.gemspec");
    if dir.join(&spec_file).is_file() {
        return dir.to_path_buf();
    }
    let nested = dir.join(name);
    if nested.join(&spec_file).is_file() {
        return nested;
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        let mut subdirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.join(&spec_file).is_file())
            .collect();
        subdirs.sort();
        if let Some(found) = subdirs.into_iter().next() {
            return found;
        }
    }
    let has_own_gemspec = gemspec_candidates_quiet(dir).is_some_and(|c| !c.is_empty());
    if !has_own_gemspec && nested.is_dir() {
        return nested;
    }
    dir.to_path_buf()
}

/// The URI Bundler's built-in git source shorthands expand to. Custom
/// `git_source` blocks cannot be evaluated statically, so they keep `key:value`.
fn git_source_uri(key: &str, value: &str) -> String {
    let with_owner = || {
        if value.contains('/') {
            value.to_string()
        } else {
            format!("{value}/{value}")
        }
    };
    match key {
        "github" => format!("https://github.com/{}.git", with_owner()),
        "gitlab" => format!("https://gitlab.com/{}.git", with_owner()),
        "gist" => format!("https://gist.github.com/{value}.git"),
        "bitbucket" => {
            let (user, repo) = value.split_once('/').unwrap_or((value, value));
            format!("https://{user}@bitbucket.org/{user}/{repo}.git")
        }
        _ => format!("{key}:{value}"),
    }
}

/// Statically evaluate an `install_if` condition. Bundler calls anything callable
/// and otherwise uses the value's truthiness; only literal bodies are decidable.
fn eval_install_condition(node: &Node<'_>) -> InstallCondition {
    fn literal(node: &Node<'_>) -> InstallCondition {
        if node.as_true_node().is_some() {
            InstallCondition::Always
        } else if node.as_false_node().is_some() || node.as_nil_node().is_some() {
            InstallCondition::Never
        } else if let Some(stmts) = node.as_statements_node() {
            let body: Vec<Node<'_>> = stmts.body().iter().collect();
            match body.as_slice() {
                [only] => literal(only),
                _ => InstallCondition::Unknown,
            }
        } else if let Some(parens) = node.as_parentheses_node() {
            parens
                .body()
                .map_or(InstallCondition::Never, |b| literal(&b))
        } else {
            InstallCondition::Unknown
        }
    }

    if let Some(lambda) = node.as_lambda_node() {
        // An empty body returns nil.
        return lambda
            .body()
            .map_or(InstallCondition::Never, |b| literal(&b));
    }
    if let Some(call) = node.as_call_node() {
        let callable = match call.name().as_slice() {
            b"lambda" | b"proc" => call.receiver().is_none(),
            b"new" => receiver_is_const(&call, b"Proc"),
            _ => false,
        };
        if callable {
            if let Some(block) = call.block().and_then(|b| b.as_block_node()) {
                return block
                    .body()
                    .map_or(InstallCondition::Never, |b| literal(&b));
            }
        }
        return InstallCondition::Unknown;
    }
    literal(node)
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
    /// `spec.version` when it is a literal, `None` when it is computed at runtime.
    version: Option<String>,
    /// Development dependency names with the 1-based line that declared them.
    development_deps: Vec<(String, usize)>,
}

impl ParsedGemspec {
    /// Bundler groups gemspec candidates by `[spec.name, spec.version]`.
    fn identity(&self) -> (String, Option<String>) {
        (
            self.name.clone(),
            self.version.as_deref().map(canonical_gem_version),
        )
    }

    fn label(&self) -> String {
        format!(
            "{}-{} ({})",
            self.name,
            self.version.as_deref().unwrap_or("unknown version"),
            self.path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        )
    }
}

/// Approximate `Gem::Version` equality: trailing zero segments are insignificant,
/// so `1.0` and `1.0.0` are the same version.
fn canonical_gem_version(version: &str) -> String {
    let mut segments: Vec<&str> = version.trim().split('.').collect();
    while segments.len() > 1 && *segments.last().unwrap() == "0" {
        segments.pop();
    }
    segments.join(".")
}

/// List the gemspecs of a directory the way Bundler's `{,*}.gemspec` glob does:
/// immediate entries only, no recursion, deterministic order.
fn gemspec_candidates(spec_dir: &Path, gemfile_path: &Path) -> Option<Vec<PathBuf>> {
    match std::fs::read_dir(spec_dir) {
        Ok(_) => gemspec_candidates_quiet(spec_dir),
        Err(e) => {
            eprintln!(
                "Warning: gemspec directive in {} points at {} which could not be read: {}",
                gemfile_path.display(),
                spec_dir.display(),
                e
            );
            None
        }
    }
}

/// `{,*}.gemspec` listing without diagnostics; `None` when `dir` is unreadable.
fn gemspec_candidates_quiet(spec_dir: &Path) -> Option<Vec<PathBuf>> {
    let entries = std::fs::read_dir(spec_dir).ok()?;
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
        let mut identities: Vec<(String, Option<String>)> = Vec::new();
        for p in &parsed {
            let id = p.identity();
            if !identities.contains(&id) {
                identities.push(id);
            }
        }
        if identities.len() > 1 {
            let listed: Vec<String> = parsed.iter().map(ParsedGemspec::label).collect();
            eprintln!(
                "Warning: gemspec directive in {} found multiple versions of `{}` in {} ({}) — using {}",
                gemfile_path.display(),
                name,
                spec_dir.display(),
                listed.join(", "),
                parsed[0].label()
            );
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

    // Bundler groups candidates by [spec.name, spec.version]: several files declaring
    // the same gem *and* version are one candidate, but the same gem at two different
    // versions is ambiguous.
    let mut identities: Vec<(String, Option<String>)> = Vec::new();
    for p in &parsed {
        let id = p.identity();
        if !identities.contains(&id) {
            identities.push(id);
        }
    }
    if identities.len() > 1 {
        let listed: Vec<String> = parsed.iter().map(ParsedGemspec::label).collect();
        eprintln!(
            "Warning: gemspec directive in {} found multiple gemspecs in {} ({}) — using {}. Pass `name:` to disambiguate.",
            gemfile_path.display(),
            spec_dir.display(),
            listed.join(", "),
            parsed[0].label()
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
        spec_version: None,
        development_deps: Vec::new(),
    };
    visitor.visit(&parse_result.node());

    Some(ParsedGemspec {
        path: spec_path.to_path_buf(),
        name: visitor
            .spec_name
            .unwrap_or_else(|| gemspec_stem(spec_path, spec_dir)),
        version: visitor.spec_version,
        development_deps: visitor.development_deps,
    })
}

/// Register the entries Bundler (>= 2.6) adds for a gemspec: the gem itself (as a
/// path source) in `:default` on all platforms, plus its development dependencies
/// in `development_group` only. Runtime dependencies are resolved transitively by
/// Bundler and are intentionally not top-level gems.
fn register_gemspec(
    ctx: &mut ParseCtx,
    spec: ParsedGemspec,
    spec_dir: &Path,
    development_group: &str,
    gemfile_path: &Path,
    line: usize,
) {
    debug_assert!(spec.path.is_file());

    let declaration = |groups: Vec<String>, file: &Path, line: usize| GemDeclaration {
        groups,
        platforms: Vec::new(),
        autorequire: true,
        install_if: InstallCondition::Always,
        file: Some(file.to_path_buf()),
        line: Some(line),
    };

    let self_groups = vec!["default".to_string()];
    ctx.gems.push(PendingGem {
        entry: GemEntry {
            name: spec.name,
            groups: self_groups.clone(),
            source: Some(GemSource::Path(spec_dir.to_path_buf())),
            declarations: vec![declaration(self_groups, gemfile_path, line)],
            ..GemEntry::default()
        },
        gemspec_dev_dep: false,
    });

    let dev_groups = vec![development_group.to_string()];
    for (dep, dep_line) in spec.development_deps {
        ctx.gems.push(PendingGem {
            entry: GemEntry {
                name: dep,
                groups: dev_groups.clone(),
                declarations: vec![declaration(dev_groups.clone(), &spec.path, dep_line)],
                ..GemEntry::default()
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
    spec_version: Option<String>,
    development_deps: Vec<(String, usize)>,
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

/// `Gem::Version.new("1.2.3")` → `1.2.3`.
fn extract_gem_version_new(node: &Node<'_>, source: &[u8]) -> Option<String> {
    let call = node.as_call_node()?;
    if call.name().as_slice() != b"new" || !receiver_is_const(&call, b"Version") {
        return None;
    }
    let first = call.arguments()?.arguments().iter().next()?;
    extract_string_value(&first, source)
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
        } else if name_bytes == b"version=" && self.is_spec_receiver(node) {
            // A computed version (e.g. `MyGem::VERSION`) stays `None`; two gemspecs are
            // only collapsed when their declared versions are known to be equal.
            if let Some(args) = node.arguments() {
                if let Some(first) = args.arguments().iter().next() {
                    self.spec_version = extract_string_value(&first, self.source)
                        .or_else(|| extract_gem_version_new(&first, self.source));
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
                                let line = line_of(self.source, node.location().start_offset());
                                self.development_deps.push((dep, line));
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

/// Group declarations by gem name.
///
/// Bundler keeps every duplicate `gem` declaration as its own dependency and
/// evaluates each one separately, so all declarations are retained on the entry.
/// The one exception mirrors Bundler's "always prefer the dependency from the
/// Gemfile": a Gemfile declaration replaces gemspec development dependencies of
/// the same name (in either order). The position of the first kept occurrence
/// is preserved.
fn merge_duplicate_gems(gems: Vec<PendingGem>) -> Vec<GemEntry> {
    let declared_in_gemfile: HashSet<String> = gems
        .iter()
        .filter(|p| !p.gemspec_dev_dep)
        .map(|p| p.entry.name.clone())
        .collect();
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut result: Vec<GemEntry> = Vec::new();

    for pending in gems {
        if pending.gemspec_dev_dep && declared_in_gemfile.contains(&pending.entry.name) {
            continue;
        }
        let gem = pending.entry;
        let idx = match seen.get(&gem.name) {
            Some(&idx) => idx,
            None => {
                seen.insert(gem.name.clone(), result.len());
                result.push(gem);
                continue;
            }
        };

        let existing = &mut result[idx];
        existing.declarations.extend(gem.declarations);
        for rp in gem.require_paths {
            if !existing.require_paths.contains(&rp) {
                existing.require_paths.push(rp);
            }
        }
        if existing.source.is_none() {
            existing.source = gem.source;
        }
    }

    for entry in &mut result {
        entry.refresh_aggregates();
    }
    result
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

/// A symbol or string literal, as Bundler `to_sym`s group and platform names.
fn extract_name(node: &Node<'_>, source: &[u8]) -> Option<String> {
    extract_symbol_value(node, source).or_else(|| extract_string_value(node, source))
}

/// `:a`, `"a"` or `[:a, "b"]` → names; anything else yields nothing.
fn extract_name_list(node: &Node<'_>, source: &[u8]) -> Vec<String> {
    if let Some(name) = extract_name(node, source) {
        return vec![name];
    }
    node.as_array_node()
        .map(|arr| {
            arr.elements()
                .iter()
                .filter_map(|el| extract_name(&el, source))
                .collect()
        })
        .unwrap_or_default()
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
    fn parse_gemfile_falls_back_to_gems_rb() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gems.rb"), "gem 'rails'\n").unwrap();
        assert_eq!(default_gemfile_path(dir.path()), dir.path().join("gems.rb"));
        assert_eq!(names(&parse_gemfile(dir.path())), vec!["rails"]);
    }

    #[test]
    fn default_gemfile_path_prefers_gemfile_over_gems_rb() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gems.rb"), "gem 'puma'\n").unwrap();
        std::fs::write(dir.path().join("Gemfile"), "gem 'rails'\n").unwrap();
        assert_eq!(default_gemfile_path(dir.path()), dir.path().join("Gemfile"));
        assert_eq!(names(&parse_gemfile(dir.path())), vec!["rails"]);
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
    fn duplicate_gems_keep_every_declaration() {
        // Previously require_false was OR'd across duplicates; Bundler requires the
        // gem if *any* declaration autorequires it.
        let gems = parse_gems("gem 'x', require: false\ngem 'x', require: 'x/init'\n");
        assert_eq!(gems.iter().filter(|g| g.name == "x").count(), 1);
        let g = find_gem(&gems, "x");
        assert_eq!(g.declarations.len(), 2);
        assert!(!g.declarations[0].autorequire);
        assert!(g.declarations[1].autorequire);
        assert!(!g.require_false);
        assert!(g.require_paths.contains(&"x/init".to_string()));
    }

    #[test]
    fn duplicate_require_false_in_dev_group_does_not_hide_default_declaration() {
        let gems = parse_gems("group :development do\n  gem 'd', require: false\nend\ngem 'd'\n");
        let g = find_gem(&gems, "d");
        assert!(g.bundler_autorequired(|group| group == "default"));
        assert!(!g.bundler_autorequired(|group| group == "development"));
    }

    #[test]
    fn duplicate_declarations_keep_their_own_platforms() {
        let gems =
            parse_gems("platforms :jruby do\n  gem 'm'\nend\ngroup :test do\n  gem 'm'\nend\n");
        let mut g = find_gem(&gems, "m").clone();
        assert_eq!(g.declarations[0].platforms, vec!["jruby"]);
        assert_eq!(g.declarations[0].groups, vec!["default"]);
        assert!(g.declarations[1].platforms.is_empty());
        assert!(g.has_platform_constraints());
        // On MRI only the :test declaration applies, so Bundler.require(:default)
        // does not load it.
        let mri: HashSet<String> = ["ruby", "mri"].iter().map(|s| s.to_string()).collect();
        g.retain_declarations_for_platform(&mri);
        assert_eq!(g.groups, vec!["test"]);
        assert!(!g.bundler_autorequired(|group| group == "default"));
    }

    #[test]
    fn declarations_record_file_and_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "source 'https://rubygems.org'\n\ngem 'rails'\neval_gemfile 'Gemfile.extra'\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("Gemfile.extra"), "\ngem 'puma'\n").unwrap();
        let gems = parse_gemfile(dir.path());
        let rails = &find_gem(&gems, "rails").declarations[0];
        assert_eq!(
            rails.file.as_deref(),
            Some(dir.path().join("Gemfile").as_path())
        );
        assert_eq!(rails.line, Some(3));
        let puma = &find_gem(&gems, "puma").declarations[0];
        assert_eq!(
            puma.file.as_deref(),
            Some(dir.path().join("Gemfile.extra").as_path())
        );
        assert_eq!(puma.line, Some(2));
    }

    // ── string groups / platforms ──

    #[test]
    fn string_group_and_platform_names_are_accepted() {
        let gems = parse_gems(
            r#"gem "strgroup", group: "test"
gem "strarr", groups: ["test", :ci]
group "development" do
  gem "strblock"
end
gem "strplat", platforms: "jruby"
platforms "jruby" do
  gem "strplatblock"
end
gem "strplatarr", platform: ["jruby", :windows]
"#,
        );
        assert_eq!(find_gem(&gems, "strgroup").groups, vec!["test"]);
        assert_eq!(find_gem(&gems, "strarr").groups, vec!["test", "ci"]);
        assert_eq!(find_gem(&gems, "strblock").groups, vec!["development"]);
        assert_eq!(find_gem(&gems, "strplat").platforms, vec!["jruby"]);
        assert_eq!(find_gem(&gems, "strplatblock").platforms, vec!["jruby"]);
        assert_eq!(
            find_gem(&gems, "strplatarr").platforms,
            vec!["jruby", "windows"]
        );
    }

    #[test]
    fn group_block_options_hash_is_not_a_group_name() {
        let gems = parse_gems("group :docs, optional: true do\n  gem 'yard'\nend\n");
        assert_eq!(find_gem(&gems, "yard").groups, vec!["docs"]);
    }

    #[test]
    fn platform_option_wins_over_platforms_option() {
        // Bundler: `opts["platform"] || opts["platforms"]`.
        let gems = parse_gems("gem 'platboth', platforms: :ruby, platform: :jruby\n");
        assert_eq!(find_gem(&gems, "platboth").platforms, vec!["jruby"]);
    }

    #[test]
    fn groups_option_wins_over_group_option() {
        // Bundler: `opts.delete("groups") || opts["group"]`.
        let gems = parse_gems("gem 'x', groups: [:test], group: :development\n");
        assert_eq!(find_gem(&gems, "x").groups, vec!["test"]);
    }

    // ── option parsing ──

    #[test]
    fn braced_options_hash_is_honored() {
        let gems = parse_gems("gem 'braces', '>= 0', { require: false, group: :test }\n");
        let g = find_gem(&gems, "braces");
        assert!(g.require_false);
        assert_eq!(g.groups, vec!["test"]);
    }

    #[test]
    fn require_nil_disables_autorequire() {
        let gems = parse_gems("gem 'reqnil', require: nil\ngem 'reqtrue', require: true\n");
        assert!(find_gem(&gems, "reqnil").require_false);
        assert!(!find_gem(&gems, "reqtrue").require_false);
    }

    #[test]
    fn install_if_literal_conditions_are_evaluated() {
        let gems = parse_gems(
            r#"gem "never_opt", install_if: -> { false }
gem "always_opt", install_if: -> { true }
gem "nil_lambda", install_if: lambda { nil }
gem "never_bool", install_if: false
gem "unknown_opt", install_if: -> { RUBY_PLATFORM.include?("darwin") }
install_if -> { false } do
  gem "never_block"
end
install_if proc { true }, -> { ENV["X"] } do
  gem "unknown_block"
end
"#,
        );
        let cond = |name: &str| find_gem(&gems, name).declarations[0].install_if;
        assert_eq!(cond("never_opt"), InstallCondition::Never);
        assert_eq!(cond("always_opt"), InstallCondition::Always);
        assert_eq!(cond("nil_lambda"), InstallCondition::Never);
        assert_eq!(cond("never_bool"), InstallCondition::Never);
        assert_eq!(cond("unknown_opt"), InstallCondition::Unknown);
        assert_eq!(cond("never_block"), InstallCondition::Never);
        assert_eq!(cond("unknown_block"), InstallCondition::Unknown);
        // Statically excluded declarations are never loaded by Bundler.require;
        // unknown conditions are conservatively treated as possibly installed.
        let default = |g: &str| g == "default";
        assert!(!find_gem(&gems, "never_block").bundler_autorequired(default));
        assert!(find_gem(&gems, "unknown_block").bundler_autorequired(default));
    }

    // ── sources ──

    #[test]
    fn git_source_shorthands_are_git_sources() {
        let gems = parse_gems(
            r#"git_source(:internal) { |repo| "https://git.example.test/#{repo}.git" }
gem "gh", github: "rails/rails"
gem "gh_short", github: "rack"
gem "gi", gist: "abc123"
gem "bb", bitbucket: "team/repo"
gem "gl", gitlab: "group/project"
gem "custom", internal: "tools/custom"
gem "plain", git: "https://example.test/plain.git"
"#,
        );
        let git = |name: &str| match &find_gem(&gems, name).source {
            Some(GemSource::Git(uri)) => uri.clone(),
            other => panic!("{name}: expected git source, got {other:?}"),
        };
        assert_eq!(git("gh"), "https://github.com/rails/rails.git");
        assert_eq!(git("gh_short"), "https://github.com/rack/rack.git");
        assert_eq!(git("gi"), "https://gist.github.com/abc123.git");
        assert_eq!(git("bb"), "https://team@bitbucket.org/team/repo.git");
        assert_eq!(git("gl"), "https://gitlab.com/group/project.git");
        assert_eq!(git("custom"), "internal:tools/custom");
        assert_eq!(git("plain"), "https://example.test/plain.git");
    }

    #[test]
    fn unknown_option_key_is_not_a_git_source() {
        let gems = parse_gems("gem 'x', internal: 'tools/x'\n");
        assert_eq!(find_gem(&gems, "x").source, None);
    }

    #[test]
    fn source_blocks_apply_to_enclosed_gems() {
        let dir = tempfile::tempdir().unwrap();
        let engine = dir.path().join("engines/foo_bar_engine");
        std::fs::create_dir_all(engine.join("lib")).unwrap();
        write_gemspec(
            &engine.join("foo_bar_engine.gemspec"),
            "  spec.name = 'foo_bar_engine'\n",
        );
        std::fs::write(
            dir.path().join("Gemfile"),
            r#"path "engines/foo_bar_engine" do
  gem "foo_bar_engine"
end
path "engines" do
  gem "foo_bar_engine_nested", require: false
end
git "https://example.test/x.git" do
  gem "from_git"
  source "https://rubygems.org" do
    gem "from_rubygems"
  end
end
github "org/repo" do
  gem "from_github"
  gem "explicit", path: "engines/foo_bar_engine"
end
gem "after_blocks"
"#,
        )
        .unwrap();
        let gems = parse_gemfile(dir.path());
        assert_eq!(
            find_gem(&gems, "foo_bar_engine").source,
            Some(GemSource::Path(engine.clone()))
        );
        // No gemspec named foo_bar_engine_nested exists: fall back to the block dir.
        assert_eq!(
            find_gem(&gems, "foo_bar_engine_nested").source,
            Some(GemSource::Path(dir.path().join("engines")))
        );
        assert_eq!(
            find_gem(&gems, "from_git").source,
            Some(GemSource::Git("https://example.test/x.git".to_string()))
        );
        assert_eq!(find_gem(&gems, "from_rubygems").source, None);
        assert_eq!(
            find_gem(&gems, "from_github").source,
            Some(GemSource::Git(
                "https://github.com/org/repo.git".to_string()
            ))
        );
        assert_eq!(
            find_gem(&gems, "explicit").source,
            Some(GemSource::Path(engine))
        );
        assert_eq!(find_gem(&gems, "after_blocks").source, None);
    }

    #[test]
    fn path_block_resolves_gem_in_subdirectory() {
        // Bundler's path source globs `{,*,*/*}.gemspec`, so a monorepo `path` block
        // finds `<dir>/<name>/<name>.gemspec`.
        let dir = tempfile::tempdir().unwrap();
        let engine = dir.path().join("engines/foo_bar_engine");
        std::fs::create_dir_all(&engine).unwrap();
        write_gemspec(
            &engine.join("foo_bar_engine.gemspec"),
            "  spec.name = 'foo_bar_engine'\n",
        );
        std::fs::write(
            dir.path().join("Gemfile"),
            "path 'engines' do\n  gem 'foo_bar_engine'\nend\ngem 'opt', path: 'engines'\n",
        )
        .unwrap();
        let gems = parse_gemfile(dir.path());
        assert_eq!(
            find_gem(&gems, "foo_bar_engine").source,
            Some(GemSource::Path(engine))
        );
    }

    #[test]
    fn path_option_expands_home_directory() {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        let gems = parse_gems("gem 'homegem', path: '~/src/homegem'\n");
        assert_eq!(
            find_gem(&gems, "homegem").source,
            Some(GemSource::Path(home.join("src/homegem")))
        );
    }

    #[test]
    fn eval_gemfile_inherits_enclosing_source_block() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "git 'https://example.test/x.git' do\n  eval_gemfile 'Gemfile.git'\nend\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("Gemfile.git"), "gem 'inner'\n").unwrap();
        let gems = parse_gemfile(dir.path());
        assert_eq!(
            find_gem(&gems, "inner").source,
            Some(GemSource::Git("https://example.test/x.git".to_string()))
        );
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
    fn canonical_gem_version_ignores_trailing_zero_segments() {
        assert_eq!(canonical_gem_version("1.0.0"), "1");
        assert_eq!(canonical_gem_version("1.0"), "1");
        assert_eq!(canonical_gem_version("1.2.0"), "1.2");
        assert_eq!(canonical_gem_version("1.2.3"), "1.2.3");
        assert_eq!(canonical_gem_version("0"), "0");
        assert_eq!(canonical_gem_version("0.0"), "0");
        assert_eq!(canonical_gem_version(" 2.1 "), "2.1");
        assert_eq!(canonical_gem_version("1.0.0.beta"), "1.0.0.beta");
    }

    fn identities_in(dir: &Path) -> Vec<(String, Option<String>)> {
        gemspec_candidates(dir, &dir.join("Gemfile"))
            .unwrap()
            .iter()
            .filter_map(|p| load_gemspec(p, dir))
            .map(|p| p.identity())
            .collect()
    }

    #[test]
    fn gemspec_identity_groups_by_name_and_version() {
        let dir = tempfile::tempdir().unwrap();
        write_gemspec(
            &dir.path().join("a.gemspec"),
            "  spec.name = 'same'\n  spec.version = '1.0.0'\n",
        );
        write_gemspec(
            &dir.path().join("b.gemspec"),
            "  spec.name = 'same'\n  spec.version = '2.0.0'\n",
        );
        let ids = identities_in(dir.path());
        assert_eq!(
            ids,
            vec![
                ("same".to_string(), Some("1".to_string())),
                ("same".to_string(), Some("2".to_string())),
            ]
        );
    }

    #[test]
    fn gemspec_identity_treats_equivalent_versions_as_one() {
        let dir = tempfile::tempdir().unwrap();
        write_gemspec(
            &dir.path().join("a.gemspec"),
            "  spec.name = 'same'\n  spec.version = '1.0'\n",
        );
        write_gemspec(
            &dir.path().join("b.gemspec"),
            "  spec.name = 'same'\n  spec.version = '1.0.0'\n",
        );
        let ids = identities_in(dir.path());
        assert_eq!(ids[0], ids[1]);
    }

    #[test]
    fn gemspec_identity_parses_gem_version_new() {
        let dir = tempfile::tempdir().unwrap();
        write_gemspec(
            &dir.path().join("a.gemspec"),
            "  spec.name = 'same'\n  spec.version = Gem::Version.new('2.1')\n",
        );
        assert_eq!(
            identities_in(dir.path()),
            vec![("same".to_string(), Some("2.1".to_string()))]
        );
    }

    #[test]
    fn gemspec_identity_leaves_computed_version_unknown() {
        let dir = tempfile::tempdir().unwrap();
        write_gemspec(
            &dir.path().join("a.gemspec"),
            "  spec.name = 'same'\n  spec.version = MyGem::VERSION\n",
        );
        assert_eq!(identities_in(dir.path()), vec![("same".to_string(), None)]);
    }

    #[test]
    fn gemspec_same_name_different_versions_selects_first_deterministically() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Gemfile"), "gemspec\n").unwrap();
        write_gemspec(
            &dir.path().join("b.gemspec"),
            "  spec.name = 'same'\n  spec.version = '2.0.0'\n  spec.add_development_dependency 'from_b'\n",
        );
        write_gemspec(
            &dir.path().join("a.gemspec"),
            "  spec.name = 'same'\n  spec.version = '1.0.0'\n  spec.add_development_dependency 'from_a'\n",
        );
        // Ambiguous (same name, different versions) → warn and use the first sorted file.
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["same", "from_a"]);
    }

    #[test]
    fn gemspec_same_name_and_version_in_multiple_files_is_not_ambiguous() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Gemfile"), "gemspec\n").unwrap();
        write_gemspec(
            &dir.path().join("a.gemspec"),
            "  spec.name = 'same'\n  spec.version = '1.0'\n  spec.add_development_dependency 'rspec'\n",
        );
        write_gemspec(
            &dir.path().join("b.gemspec"),
            "  spec.name = 'same'\n  spec.version = '1.0.0'\n",
        );
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["same", "rspec"]);
    }

    #[test]
    fn gemspec_name_option_with_conflicting_versions_selects_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Gemfile"), "gemspec name: 'same'\n").unwrap();
        write_gemspec(
            &dir.path().join("b.gemspec"),
            "  spec.name = 'same'\n  spec.version = '2.0.0'\n  spec.add_development_dependency 'from_b'\n",
        );
        write_gemspec(
            &dir.path().join("a.gemspec"),
            "  spec.name = 'same'\n  spec.version = '1.0.0'\n  spec.add_development_dependency 'from_a'\n",
        );
        let gems = parse_gemfile(dir.path());
        assert_eq!(names(&gems), vec!["same", "from_a"]);
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
    fn gemspec_inside_group_block_ignores_enclosing_context() {
        // Bundler >= 2.6: the gemspec gem is added via add_dependency, so enclosing
        // group/platforms blocks do not apply; dev deps get only development_group.
        // (This test previously asserted the pre-2.6 inheritance.)
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "group :ci do\n  platforms :jruby do\n    gemspec\n  end\nend\n",
        )
        .unwrap();
        write_gemspec(
            &dir.path().join("foo.gemspec"),
            "  spec.name = 'my_gem'\n  spec.add_development_dependency 'rspec'\n",
        );
        let gems = parse_gemfile(dir.path());
        let my_gem = find_gem(&gems, "my_gem");
        assert_eq!(my_gem.groups, vec!["default"]);
        assert!(my_gem.platforms.is_empty());
        let rspec = find_gem(&gems, "rspec");
        assert_eq!(rspec.groups, vec!["development"]);
        assert!(rspec.platforms.is_empty());
        assert_eq!(
            rspec.declarations[0].file.as_deref(),
            Some(dir.path().join("foo.gemspec").as_path())
        );
        assert_eq!(rspec.declarations[0].line, Some(3));
        assert_eq!(my_gem.declarations[0].line, Some(3));
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
