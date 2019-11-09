// Copyright 2018 The Starlark in Rust Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The enviroment, called "Module" in [this spec](
//! https://github.com/google/skylark/blob/a0e5de7e63b47e716cca7226662a4c95d47bf873/doc/spec.md)
//! is the list of variable in the current scope. It can be frozen, after which all values from
//! this environment become immutable.

use crate::gc::Heap;
use crate::values::error::{RuntimeError, ValueError};
use crate::values::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

// TODO: move that code in some common error code list?
// CM prefix = Critical Module
const FROZEN_ENV_ERROR_CODE: &str = "CM00";
const NOT_FOUND_ERROR_CODE: &str = "CM01";
const LOCAL_VARIABLE_REFERENCED_BEFORE_ASSIGNMENT: &str = "CM03";
pub(crate) const LOAD_NOT_SUPPORTED_ERROR_CODE: &str = "CM02";
const CANNOT_IMPORT_ERROR_CODE: &str = "CE02";

#[derive(Debug)]
#[doc(hidden)]
pub enum EnvironmentError {
    /// Raised when trying to change a variable on a frozen environment.
    TryingToMutateFrozenEnvironment,
    /// Variables was no found.
    VariableNotFound(String),
    LocalVariableReferencedBeforeAssignment(String),
    /// Cannot import private symbol, i.e. underscore prefixed
    CannotImportPrivateSymbol(String),
}

impl Into<RuntimeError> for EnvironmentError {
    fn into(self) -> RuntimeError {
        RuntimeError {
            code: match self {
                EnvironmentError::TryingToMutateFrozenEnvironment => FROZEN_ENV_ERROR_CODE,
                EnvironmentError::VariableNotFound(..) => NOT_FOUND_ERROR_CODE,
                EnvironmentError::CannotImportPrivateSymbol(..) => CANNOT_IMPORT_ERROR_CODE,
                EnvironmentError::LocalVariableReferencedBeforeAssignment(..) => {
                    LOCAL_VARIABLE_REFERENCED_BEFORE_ASSIGNMENT
                }
            },
            label: match self {
                EnvironmentError::TryingToMutateFrozenEnvironment => {
                    "This value belong to a frozen environment".to_owned()
                }
                EnvironmentError::VariableNotFound(..) => "Variable was not found".to_owned(),
                EnvironmentError::LocalVariableReferencedBeforeAssignment(..) => {
                    "Local variable referenced before assignment".to_owned()
                }
                EnvironmentError::CannotImportPrivateSymbol(ref s) => {
                    format!("Symbol '{}' is private", s)
                }
            },
            message: match self {
                EnvironmentError::TryingToMutateFrozenEnvironment => {
                    "Cannot mutate a frozen environment".to_owned()
                }
                EnvironmentError::VariableNotFound(s) => format!("Variable '{}' not found", s),
                EnvironmentError::LocalVariableReferencedBeforeAssignment(ref s) => {
                    format!("Local variable '{}' referenced before assignment", s)
                }
                EnvironmentError::CannotImportPrivateSymbol(s) => {
                    format!("Cannot import private symbol '{}'", s)
                }
            },
        }
    }
}

impl From<EnvironmentError> for ValueError {
    fn from(e: EnvironmentError) -> Self {
        ValueError::Runtime(e.into())
    }
}

#[derive(Clone, Debug)]
pub struct Environment {
    env: Rc<RefCell<EnvironmentContent>>,
}

#[derive(Debug)]
struct EnvironmentContent {
    /// A name for this environment, used mainly for debugging.
    name_: String,
    /// Whether the environment is frozen or not.
    frozen: bool,
    /// Super environment that represent a higher scope than the current one
    parent: Option<Environment>,
    /// Symbols imported from these environments.
    /// Explicitly keep references to prevent running GC on these.
    deps: HashMap<*const (), Environment>,
    /// Where objects are garbage collected
    heap: Heap,
    /// List of variable bindings
    ///
    /// `bool` indicates whether value belongs to this environment (`true`) or imported (`false`)
    variables: HashMap<String, (Value, bool)>,
    /// Optional function which can be used to construct set literals (i.e. `{foo, bar}`).
    /// If not set, attempts to use set literals will raise an error.
    set_constructor: SetConstructor,
}

// Newtype so that EnvironmentContent can derive Debug.
struct SetConstructor(Option<Box<dyn Fn(Vec<Value>) -> ValueResult>>);

impl std::fmt::Debug for SetConstructor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        if self.0.is_some() {
            write!(f, "<set constructor>")
        } else {
            write!(f, "<no set constructor>")
        }
    }
}

impl Environment {
    /// Create a new environment
    pub fn new(name: &str) -> Environment {
        Environment {
            env: Rc::new(RefCell::new(EnvironmentContent {
                name_: name.to_owned(),
                frozen: false,
                parent: None,
                deps: HashMap::new(),
                heap: Heap::new(name),
                variables: HashMap::new(),
                set_constructor: SetConstructor(None),
            })),
        }
    }

    /// Create a new child environment for this environment
    pub fn child(&self, name: &str) -> Environment {
        self.freeze(true);
        Environment {
            env: Rc::new(RefCell::new(EnvironmentContent {
                name_: name.to_owned(),
                frozen: false,
                parent: Some(self.clone()),
                deps: HashMap::new(),
                heap: Heap::new(name),
                variables: HashMap::new(),
                set_constructor: SetConstructor(None),
            })),
        }
    }

    /// Get a heap which stores known objects for this environment.
    pub(crate) fn heap(&self) -> Heap {
        self.env.borrow().heap.clone()
    }

    /// Create a new child environment
    /// Freeze the environment, all its value will become immutable after that
    pub fn freeze(&self, gc: bool) -> &Self {
        if self.env.borrow_mut().freeze() {
            let heap = self.env.borrow().heap.clone();
            if gc {
                // Note GC on freeze is optional: if it is not called, objects will be dropped
                // during the heap drop without graph traversal (much faster).
                heap.gc(self, "freeze");
            } else {
                heap.gc_weak("freeze");
            }
        }
        self
    }

    /// Return the name of this module
    pub fn name(&self) -> String {
        self.env.borrow().name_.clone()
    }

    /// Set the value of a variable in that environment.
    pub fn set(&self, name: &str, value: Value) -> Result<(), EnvironmentError> {
        self.env.borrow_mut().set(name, value, false)
    }

    /// Get the value of the variable `name`
    pub fn get(&self, name: &str) -> Result<Value, EnvironmentError> {
        self.env.borrow().get(name)
    }

    /// Add environment as a dependency
    fn add_env_dep(&self, dep: &Environment) {
        self.env
            .borrow_mut()
            .deps
            .insert(dep.env.as_ptr() as *const (), dep.clone());
    }

    pub fn import_symbol(
        &self,
        env: &Environment,
        symbol: &str,
        new_name: &str,
    ) -> Result<(), EnvironmentError> {
        let first = symbol.chars().next();
        match first {
            Some('_') | None => Err(EnvironmentError::CannotImportPrivateSymbol(
                symbol.to_owned(),
            )),
            _ => {
                self.add_env_dep(env);
                self.env.borrow_mut().set(new_name, env.get(symbol)?, true)
            }
        }
    }

    /// Return the parent environment (or `None` if there is no parent).
    pub fn get_parent(&self) -> Option<Environment> {
        self.env.borrow().get_parent()
    }

    /// Set the function which will be used to instantiate set literals encountered when evaluating
    /// in this `Environment`. Set literals are {}s with one or more elements between, separated by
    /// commas, e.g. `{1, 2, "three"}`.
    ///
    /// If this function is not called on the `Environment`, its parent's set constructor will be
    /// used when set literals are encountered. If neither this `Environment`, nor any of its
    /// transitive parents, have a set constructor defined, attempts to evaluate set literals will
    /// raise and error.
    ///
    /// The `Value` returned by this function is expected to be a one-dimensional collection
    /// containing no duplicates.
    pub fn with_set_constructor(&self, constructor: Box<dyn Fn(Vec<Value>) -> ValueResult>) {
        self.env.borrow_mut().set_constructor = SetConstructor(Some(constructor));
    }

    pub(crate) fn make_set(&self, values: Vec<Value>) -> ValueResult {
        match self.env.borrow().set_constructor.0 {
            Some(ref ctor) => ctor(values),
            None => {
                if let Some(parent) = self.get_parent() {
                    parent.make_set(values)
                } else {
                    Err(ValueError::TypeNotSupported("set".to_string()))
                }
            }
        }
    }

    /** GC roots */
    pub(crate) fn roots(&self) -> Vec<ValueGcStrong> {
        let content = self.env.borrow();
        content
            .variables
            .values()
            .flat_map(|(v, imported)| if !imported { Some(v) } else { None })
            .flat_map(Value::to_gc_strong)
            .collect()
    }
}

impl EnvironmentContent {
    /// Create a new child environment
    /// Freeze the environment, all its value will become immutable after that
    pub fn freeze(&mut self) -> bool {
        if !self.frozen {
            self.frozen = true;
            for v in self.variables.values_mut() {
                v.0.freeze();
            }
            true
        } else {
            false
        }
    }

    /// Set the value of a variable in that environment.
    pub fn set(
        &mut self,
        name: &str,
        value: Value,
        imported: bool,
    ) -> Result<(), EnvironmentError> {
        if self.frozen {
            Err(EnvironmentError::TryingToMutateFrozenEnvironment)
        } else {
            self.variables.insert(name.to_string(), (value, imported));
            Ok(())
        }
    }

    /// Get the value of the variable `name`
    pub fn get(&self, name: &str) -> Result<Value, EnvironmentError> {
        if self.variables.contains_key(name) {
            Ok(self.variables[name].0.clone())
        } else {
            match self.parent {
                Some(ref p) => p.get(name),
                None => Err(EnvironmentError::VariableNotFound(name.to_owned())),
            }
        }
    }

    /// Return the parent environment (or `None` if there is no parent).
    pub fn get_parent(&self) -> Option<Environment> {
        self.parent.clone()
    }
}

/// Environment passed to `call` calls.
///
/// Function implementations are only allowed to access
/// type values from "type values" from the caller context,
/// so this struct is passed instead of full `Environment`.
#[derive(Clone)]
pub struct TypeValues {
    /// Type values live here
    heap: Heap,
    /// List of static values of an object per type
    type_objs: HashMap<String, HashMap<String, Value>>,
}

impl Default for TypeValues {
    fn default() -> TypeValues {
        TypeValues {
            heap: Heap::new("type_values"),
            type_objs: Default::default(),
        }
    }
}

impl TypeValues {
    /// Get a type value if it exists (e.g. list.index).
    pub fn get_type_value(&self, obj: &Value, id: &str) -> Option<Value> {
        self.type_objs
            .get(obj.get_type())
            .and_then(|o| o.get(id))
            .cloned()
    }

    /// List the attribute of a type
    pub fn list_type_value(&self, obj: &Value) -> Vec<String> {
        self.type_objs
            .get(obj.get_type())
            .into_iter()
            .flat_map(|o| o.keys().cloned())
            .collect()
    }

    /// Get the object of type `obj_type`, and create it if none exists
    pub fn add_type_value(&mut self, obj: &str, attr: &str, value: Value) {
        if let Some(ref mut v) = self.type_objs.get_mut(obj) {
            v.insert(attr.to_owned(), value);
        } else {
            let mut dict = HashMap::new();
            dict.insert(attr.to_owned(), value);
            self.type_objs.insert(obj.to_owned(), dict);
        }
    }

    pub(crate) fn heap(&self) -> Heap {
        self.heap.clone()
    }
}
