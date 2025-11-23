//! Module dependency resolution
//!
//! Handles dependency checking and resolution order for modules.

use std::collections::{HashMap, VecDeque};
use tracing::debug;

use crate::module::registry::discovery::DiscoveredModule;
use crate::module::traits::ModuleError;

/// Dependency resolution result
#[derive(Debug, Clone)]
pub struct DependencyResolution {
    /// Modules in load order (dependencies first)
    pub load_order: Vec<String>,
    /// Module dependencies map
    pub dependencies: HashMap<String, Vec<String>>,
    /// Missing dependencies
    pub missing: Vec<String>,
}

/// Dependency resolver
pub struct ModuleDependencies;

impl ModuleDependencies {
    /// Resolve module dependencies and determine load order
    pub fn resolve(
        discovered_modules: &[DiscoveredModule],
    ) -> Result<DependencyResolution, ModuleError> {
        let mut module_map: HashMap<String, &DiscoveredModule> = HashMap::new();
        for module in discovered_modules {
            module_map.insert(module.manifest.name.clone(), module);
        }

        // Build dependency graph
        let mut dependencies: HashMap<String, Vec<String>> = HashMap::new();
        let mut missing = Vec::new();

        for module in discovered_modules {
            let module_deps: Vec<String> = module.manifest.dependencies.keys().cloned().collect();

            // Check if all dependencies are available
            for dep in &module_deps {
                if !module_map.contains_key(dep) {
                    missing.push(dep.clone());
                }
            }

            dependencies.insert(module.manifest.name.clone(), module_deps);
        }

        if !missing.is_empty() {
            return Err(ModuleError::DependencyMissing(format!(
                "Missing dependencies: {missing:?}"
            )));
        }

        // Topological sort to determine load order
        let load_order = Self::topological_sort(&dependencies).map_err(|e| {
            ModuleError::DependencyMissing(format!("Circular dependency detected: {e}"))
        })?;

        debug!("Dependency resolution complete: {:?}", load_order);

        Ok(DependencyResolution {
            load_order,
            dependencies,
            missing: Vec::new(),
        })
    }

    /// Topological sort of dependencies
    fn topological_sort(
        dependencies: &HashMap<String, Vec<String>>,
    ) -> Result<Vec<String>, String> {
        let mut in_degree: HashMap<String, usize> = HashMap::new();
        let mut graph: HashMap<String, Vec<String>> = HashMap::new();

        // Initialize in-degree
        for module in dependencies.keys() {
            in_degree.insert(module.clone(), 0);
        }

        // Build reverse graph and calculate in-degrees
        for (module, deps) in dependencies {
            for dep in deps {
                graph.entry(dep.clone()).or_default().push(module.clone());
                *in_degree.get_mut(module).unwrap() += 1;
            }
        }

        // Kahn's algorithm for topological sort
        let mut queue: VecDeque<String> = VecDeque::new();
        for (module, &degree) in &in_degree {
            if degree == 0 {
                queue.push_back(module.clone());
            }
        }

        let mut result = Vec::new();
        let mut count = 0;

        while let Some(module) = queue.pop_front() {
            result.push(module.clone());
            count += 1;

            if let Some(dependents) = graph.get(&module) {
                for dependent in dependents {
                    let degree = in_degree.get_mut(dependent).unwrap();
                    *degree -= 1;
                    if *degree == 0 {
                        queue.push_back(dependent.clone());
                    }
                }
            }
        }

        // Check for circular dependencies
        if count != dependencies.len() {
            return Err("Circular dependency detected".to_string());
        }

        Ok(result)
    }
}
