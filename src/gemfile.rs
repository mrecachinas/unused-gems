use std::path::Path;

use ruby_prism::{self, Node, Visit};

use crate::types::GemEntry;

/// Parse a Gemfile using ruby-prism AST and extract gem declarations.
pub fn parse_gemfile(dir: &Path) -> Vec<GemEntry> {
    let gemfile_path = dir.join("Gemfile");
    if !gemfile_path.exists() {
        eprintln!("Error: No Gemfile found in {}", dir.display());
        std::process::exit(1);
    }

    let mut gems = Vec::new();
    let mut files_to_process = vec![dir.join("Gemfile")];
    let mut processed_files = std::collections::HashSet::new();

    while let Some(path) = files_to_process.pop() {
        if !path.exists() {
            continue;
        }
        if !processed_files.insert(path.clone()) {
            continue;
        }

        let source = match std::fs::read(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Warning: Could not read Gemfile {}: {}", path.display(), e);
                continue;
            }
        };

        let parse_result = ruby_prism::parse(&source);

        let mut visitor = GemfileVisitor {
            gems: Vec::new(),
            current_groups: Vec::new(),
            source: &source,
            eval_gemfiles: Vec::new(),
            gemspec_paths: Vec::new(),
        };
        visitor.visit(&parse_result.node());

        gems.extend(visitor.gems);

        let parent = path.parent().unwrap_or(dir);
        for eval_path in visitor.eval_gemfiles {
            let p = parent.join(eval_path);
            files_to_process.push(p);
        }

        // Collect gemspec path: directives relative to the Gemfile's directory
        for gpath in visitor.gemspec_paths {
            let spec_dir = parent.join(&gpath);
            let pattern = spec_dir.join("*.gemspec").display().to_string();
            for entry in glob::glob(&pattern).into_iter().flatten().flatten() {
                parse_gemspec_deps(&entry, &mut gems);
            }
        }
    }

    // Also parse gemspec in the root directory (default gemspec with no path: arg)
    let pattern = dir.join("*.gemspec").display().to_string();
    for entry in glob::glob(&pattern).into_iter().flatten().flatten() {
        parse_gemspec_deps(&entry, &mut gems);
    }

    gems
}

struct GemfileVisitor<'s> {
    gems: Vec<GemEntry>,
    current_groups: Vec<String>,
    source: &'s [u8],
    eval_gemfiles: Vec<String>,
    gemspec_paths: Vec<String>,
}

impl<'s> GemfileVisitor<'s> {
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
                                    for el in arr.elements().iter() {
                                        if let Some(s) = extract_string_value(&el, self.source) {
                                            require_paths.push(s);
                                        }
                                    }
                                }
                            }
                            Some("group") | Some("groups") => {
                                if let Some(sym) =
                                    extract_symbol_value(&assoc.value(), self.source)
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
                            _ => {}
                        }
                    }
                }
            }
        }

        if groups.is_empty() {
            groups.push("default".to_string());
        }

        self.gems.push(GemEntry {
            name,
            groups,
            require_paths: if require_false { Vec::new() } else { require_paths },
            module_names: Vec::new(),
            require_false,
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
}

impl<'s> Visit<'_> for GemfileVisitor<'s> {
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
        } else if name_bytes == b"eval_gemfile" && node.receiver().is_none() {
             if let Some(args) = node.arguments() {
                if let Some(first) = args.arguments().iter().next() {
                    if let Some(path) = extract_string_value(&first, self.source) {
                        self.eval_gemfiles.push(path);
                    }
                }
             }
        } else if name_bytes == b"gemspec" && node.receiver().is_none() {
            // Extract path: argument for gemspec directives
            if let Some(args) = node.arguments() {
                for arg in args.arguments().iter() {
                    if let Some(hash) = arg.as_keyword_hash_node() {
                        for elem in hash.elements().iter() {
                            if let Some(assoc) = elem.as_assoc_node() {
                                let key_name = extract_key_name(&assoc.key(), self.source);
                                if key_name.as_deref() == Some("path") {
                                    if let Some(p) = extract_string_value(&assoc.value(), self.source) {
                                        self.gemspec_paths.push(p);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        ruby_prism::visit_call_node(self, node);
    }
}

/// Parse a .gemspec file for dependencies.
fn parse_gemspec_deps(spec_path: &Path, gems: &mut Vec<GemEntry>) {
    let source = match std::fs::read(spec_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "Warning: Could not read gemspec {}: {}",
                spec_path.display(),
                e
            );
            return;
        }
    };

    let parse_result = ruby_prism::parse(&source);

    let existing_names: std::collections::HashSet<String> =
        gems.iter().map(|g| g.name.clone()).collect();

    let mut visitor = GemspecVisitor {
        gems: Vec::new(),
        source: &source,
    };
    visitor.visit(&parse_result.node());

    for gem in visitor.gems {
        if !existing_names.contains(&gem.name) {
            gems.push(gem);
        }
    }
}

struct GemspecVisitor<'s> {
    gems: Vec<GemEntry>,
    source: &'s [u8],
}

impl<'s> Visit<'_> for GemspecVisitor<'s> {
    fn visit_call_node(&mut self, node: &ruby_prism::CallNode<'_>) {
        let name = node.name();
        let name_bytes = name.as_slice();

        let groups = match name_bytes {
            b"add_dependency" | b"add_runtime_dependency" => vec!["default".to_string()],
            b"add_development_dependency" => vec!["development".to_string()],
            _ => {
                ruby_prism::visit_call_node(self, node);
                return;
            }
        };

        if let Some(args) = node.arguments() {
            let arg_list: Vec<Node<'_>> = args.arguments().iter().collect();
            if let Some(first) = arg_list.first() {
                if let Some(dep_name) = extract_string_value(first, self.source) {
                    self.gems.push(GemEntry {
                        name: dep_name,
                        groups,
                        require_paths: Vec::new(),
                        module_names: Vec::new(),
                        require_false: false,
                    });
                }
            }
        }

        ruby_prism::visit_call_node(self, node);
    }
}

// --- Helper functions for extracting values from Prism AST nodes ---

fn extract_string_value(node: &Node<'_>, _source: &[u8]) -> Option<String> {
    if let Some(s) = node.as_string_node() {
        return Some(String::from_utf8_lossy(s.unescaped().as_ref()).to_string());
    }
    None
}

fn extract_symbol_value(node: &Node<'_>, _source: &[u8]) -> Option<String> {
    if let Some(s) = node.as_symbol_node() {
        return Some(String::from_utf8_lossy(s.unescaped().as_ref()).to_string());
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
        gems.iter().find(|g| g.name == name).unwrap_or_else(|| panic!("gem '{}' not found", name))
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
        let names: Vec<&str> = gems.iter().map(|g| g.name.as_str()).collect();
        assert!(names.contains(&"rails"));
        assert!(names.contains(&"puma"));
        assert!(names.contains(&"pg"));
    }

    #[test]
    fn gemspec_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Gemfile"), "source 'https://rubygems.org'\ngemspec\n").unwrap();
        std::fs::write(dir.path().join("foo.gemspec"), r#"
Gem::Specification.new do |s|
  s.name = 'foo'
  s.add_dependency 'faraday'
  s.add_development_dependency 'rspec'
end
"#).unwrap();
        let gems = parse_gemfile(dir.path());
        let faraday = find_gem(&gems, "faraday");
        assert_eq!(faraday.groups, vec!["default"]);
        let rspec = find_gem(&gems, "rspec");
        assert_eq!(rspec.groups, vec!["development"]);
    }

    #[test]
    fn eval_gemfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Gemfile"), "eval_gemfile('Gemfile.shared')\ngem 'rails'\n").unwrap();
        std::fs::write(dir.path().join("Gemfile.shared"), "gem 'puma'\n").unwrap();
        let gems = parse_gemfile(dir.path());
        assert_eq!(gems.len(), 2);
        let names: Vec<&str> = gems.iter().map(|g| g.name.as_str()).collect();
        assert!(names.contains(&"rails"));
        assert!(names.contains(&"puma"));
    }

    #[test]
    fn gemspec_with_path() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("engines/core");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.path().join("Gemfile"), "gemspec path: 'engines/core'\n").unwrap();
        std::fs::write(sub.join("core.gemspec"), r#"
Gem::Specification.new do |s|
  s.add_dependency 'activesupport'
end
"#).unwrap();
        let gems = parse_gemfile(dir.path());
        assert!(gems.iter().any(|g| g.name == "activesupport"));
    }

    #[test]
    fn deduplicates_gemspec_and_gemfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Gemfile"), "gemspec\ngem 'faraday'\n").unwrap();
        std::fs::write(dir.path().join("foo.gemspec"), r#"
Gem::Specification.new do |s|
  s.add_dependency 'faraday'
end
"#).unwrap();
        let gems = parse_gemfile(dir.path());
        let faraday_count = gems.iter().filter(|g| g.name == "faraday").count();
        // Should appear once from Gemfile, gemspec dep skipped because it already exists
        assert_eq!(faraday_count, 1);
    }
}
