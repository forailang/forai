use std::io::{BufRead, Write};

// ── JSON-RPC types ────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct RpcRequest {
    #[allow(dead_code)]
    #[serde(default)]
    jsonrpc: String,
    id: Option<serde_json::Value>,
    method: String,
    params: Option<serde_json::Value>,
}

#[derive(serde::Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    id: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(serde::Serialize)]
struct RpcError {
    code: i32,
    message: String,
}

impl RpcResponse {
    fn ok(id: serde_json::Value, result: serde_json::Value) -> Self {
        RpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }
    fn err(id: serde_json::Value, code: i32, message: impl Into<String>) -> Self {
        RpcResponse {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

// ── Tool registry ─────────────────────────────────────────────────────

struct ToolDef {
    name: &'static str,
    description: &'static str,
    schema: &'static str,
}

fn all_tools() -> &'static [ToolDef] {
    &[
        ToolDef {
            name: "fai_fmt",
            description: "Format all source files in the fai project.",
            schema: r#"{"type":"object","properties":{"project_dir":{"type":"string","description":"Absolute path to the fai project root directory."}},"required":["project_dir"]}"#,
        },
        ToolDef {
            name: "fai_check",
            description: "Format then type-check the fai project.",
            schema: r#"{"type":"object","properties":{"project_dir":{"type":"string","description":"Absolute path to the fai project root directory."}},"required":["project_dir"]}"#,
        },
        ToolDef {
            name: "fai_test",
            description: "Format, type-check, then run all tests in the fai project.",
            schema: r#"{"type":"object","properties":{"project_dir":{"type":"string","description":"Absolute path to the fai project root directory."}},"required":["project_dir"]}"#,
        },
        ToolDef {
            name: "fai_run",
            description: "Format, type-check, test, then run the fai project.",
            schema: r#"{"type":"object","properties":{"project_dir":{"type":"string","description":"Absolute path to the fai project root directory."}},"required":["project_dir"]}"#,
        },
        ToolDef {
            name: "fai_build",
            description: "Format, type-check, test, then build the fai project. For single-target projects this produces a .wasm file. For multi-target projects (fai.toml with [project.client] / [project.server] sections) omit 'target' to build ALL targets, or pass a sub-project name (e.g. 'client' or 'server') to build one. Do NOT pass 'wasm', 'wasm-html', or 'native' as the target — those are output formats, not sub-project names.",
            schema: r#"{"type":"object","properties":{"project_dir":{"type":"string","description":"Absolute path to the fai project root directory."},"target":{"type":"string","description":"Optional sub-project name to build (e.g. 'client' or 'server'). Omit to build all targets. Do NOT pass output-format names like 'wasm', 'wasm-html', or 'native' here."}},"required":["project_dir"]}"#,
        },
        ToolDef {
            name: "fai_doc",
            description: "Search or browse fai documentation. Covers the language reference, stdlib, project functions, and package docs. Call with no query to list all top-level namespaces. Drill down with dot-notation: 'lang' lists language topics, 'lang.modules' shows all module/import docs, 'lang.modules.rpc_server' shows that specific topic. 'std' lists stdlib modules, 'std.array' lists array functions, 'std.array.length' shows length docs. Package names (e.g. 'Forui') show the package overview and sub-modules.",
            schema: r#"{"type":"object","properties":{"project_dir":{"type":"string","description":"Absolute path to the fai project root directory."},"query":{"type":"string","description":"Documentation query. Examples: '' (list all), 'lang' (language overview), 'lang.modules' (import patterns), 'lang.modules.rpc_server' (specific topic), 'std.array' (array module), 'fontSize' (search by name), 'Forui' (package overview)."}},"required":["project_dir"]}"#,
        },
        ToolDef {
            name: "fai_examples",
            description: "Get complete working code examples for common forai patterns. Call with no query to list all available examples. Pass a keyword to get a specific example. Keywords: 'fai.toml', 'fullstack', 'rpc', 'http', 'function', 'types', 'testing', 'ui', 'children'.",
            schema: r#"{"type":"object","properties":{"project_dir":{"type":"string","description":"Absolute path to the fai project root directory."},"query":{"type":"string","description":"Keyword to find an example. Examples: 'fai.toml' for project config, 'rpc' for RPC server+client, 'http' for HTTP+JSON, 'testing' for test patterns, 'ui' for UI component tests, 'children' for custom components that take a do...end block. Omit to list all examples."}},"required":["project_dir"]}"#,
        },
        ToolDef {
            name: "fai_new",
            description: "Scaffold a new fai project in the given parent directory.",
            schema: r#"{"type":"object","properties":{"parent_dir":{"type":"string","description":"Absolute path to the directory where the new project will be created."},"name":{"type":"string","description":"Name of the new project."}},"required":["parent_dir","name"]}"#,
        },
    ]
}

// ── Examples registry ─────────────────────────────────────────────────

struct ExampleDef {
    name: &'static str,
    keywords: &'static str, // space-separated, for matching
    description: &'static str,
    content: &'static str,
}

const EXAMPLE_FAI_TOML_SINGLE: &str = r#"
# fai.toml — single-target project (WASM or native executable)

[project]
name = "my-app"
version = "0.1.0"
source_root = "src"

[dependencies]
Forui = "file:///path/to/forui"
HtmlForui = "file:///path/to/html-forui"

# Or fetch from a public git repo:
# Forui = "https://github.com/forailang/forui"
# HtmlForui = "https://github.com/forailang/html-forui"
"#;

const EXAMPLE_FAI_TOML_FULLSTACK: &str = r#"
# fai.toml — fullstack multi-target project (WASM client + native server)
# Both targets share the same source root so they can import from each other.

[project]
name = "my-fullstack-app"
version = "0.1.0"
source_root = "src"

[project.client]
target = "wasm-html"
source = "src"
main = "src/client/main.fai"
build_dir = "build/client"

[project.server]
target = "native"
source = "src"
main = "src/server/main.fai"
build_dir = "build/server"

[dependencies]
Forui = "file:///path/to/forui"
HtmlForui = "file:///path/to/html-forui"

# Or fetch from a public git repo:
# Forui = "https://github.com/forailang/forui"
# HtmlForui = "https://github.com/forailang/html-forui"

# Client calls server via RPC at this URL:
[project.client.dependencies.server.remote.dev]
url = "http://localhost:3040"
"#;

const EXAMPLE_RPC_SERVER: &str = r#"
# RPC Server — src/server/main.fai
#
# Mark types with `remote type` and functions with `remote def` to expose them.
# `addRpcRoutes` is AUTO-GENERATED by `fai build` — do NOT define it yourself.
# `use { handleRpcRequest } from Forui.rpc` is required.

use std.array
use std.http.server
use { handleRpcRequest } from Forui.rpc
use { App } from client                      # import client for SSR (optional)

remote type Task                              # exported to client proxy
  id Int
  text String
  done Bool
end

var tasks Task[] = []
var nextId = 1

remote def getTasks                           # exported to client proxy
    @return Task[]
do
  tasks
end

remote def addTask
    @param text String
    @return Task
do
  let t = Task(id: nextId, text: text, done: false)
  nextId = nextId + 1
  tasks = array.append(tasks, t)
  t
end

remote def toggleTask
    @param id Int
    @return Task
do
  var result Task? = null
  var updated Task[] = []
  for t in tasks
    if t.id == id
      let tog = Task(id: t.id, text: t.text, done: !t.done)
      result = tog
      updated = array.append(updated, tog)
    else
      updated = array.append(updated, t)
    end
  end
  tasks = updated
  result!
end

def main
    @return Void
do
  var r = server.router()
  addRpcRoutes(r)                             # auto-generated — do NOT define this
  server.listen(r, 3040)
end
"#;

const EXAMPLE_RPC_CLIENT: &str = r#"
# RPC Client — src/client/pages/tasks.fai
#
# Import from the auto-generated `Server` module (created by `fai build`).
# Every signal must be `var` — let bindings cannot be mutated.
# Every signal helper (isLoading, isError, reload, setValue) must be imported.

use { Task, getTasks, addTask, toggleTask } from Server
use { useSignal, isLoading, isError, reload, setValue } from Forui.signal
use { ViewNode, VStack, HStack, Label, Button, TextInput,
      fontSize, foreground, padding } from Forui.view

def TasksPage
    @return ViewNode
do
  var tasks = useSignal([]) do
    getTasks()                                # loader — called automatically
  end
  var input = useSignal('')

  VStack do
    if tasks.isLoading()
      Label('Loading…').foreground('#888')
    else if tasks.isError()
      Label('Error: ' + tasks.error!).foreground('#e00')
    else
      for t in tasks.value
        HStack do
          Label(t.text)
          Button(if t.done 'Undo' else 'Done' end, onClick: do
            toggleTask(t.id)
            tasks.reload()
          end)
        end
      end
      HStack do
        TextInput('New task…', signalValue: input)
        Button('Add', onClick: do
          if input.value != ''
            addTask(input.value)
            input.setValue('')
            tasks.reload()
          end
        end)
      end
    end
  end
end

test TasksPage
it 'renders without crashing'
  let tree = testMount(TasksPage)
  assert.equals(tree.kind, 'VStack')
end
end
"#;

const EXAMPLE_HTTP_JSON: &str = r#"
# HTTP + JSON — fetching data and parsing into typed structs
#
# Use std.http.request for HTTP calls.
# Use getString / getInt / getBool to extract fields from parsed JSON (all return Optional).
# For hyphenated headers (x-api-key etc.) use dictionary.set — literal keys must be identifiers.

use std.http.request
use std.json
use std.array

type Team
  id String
  name String
  playerCount Int
end

# Fetch a single object and parse it into a typed struct.
def fetchTeam
    @param apiKey String
    @param teamId String
    @return Team
do
  var headers = {}
  headers = set(headers, 'x-api-key', apiKey)       # hyphenated header via set
  let res = request.get(
    "https://api.example.com/teams/{{teamId}}",
    headers: headers
  )
  let body = json.parse(res.body)
  Team(
    id:          unwrap(getString(body, 'id'), ''),
    name:        unwrap(getString(body, 'name'), ''),
    playerCount: unwrap(getInt(body, 'player_count'), 0),
  )
end

# Fetch a JSON array and parse each item into a list.
def fetchTeams
    @param apiKey String
    @return Team[]
do
  var headers = {}
  headers = set(headers, 'x-api-key', apiKey)
  let res = request.get('https://api.example.com/teams', headers: headers)
  let data = json.parse(res.body)                    # returns Dictionary (JSON array)
  var teams Team[] = []
  for item in data.items                             # iterate array items
    teams = array.append(teams, Team(
      id:          unwrap(getString(item, 'id'), ''),
      name:        unwrap(getString(item, 'name'), ''),
      playerCount: unwrap(getInt(item, 'player_count'), 0),
    ))
  end
  teams
end

# POST with a JSON body.
def createTeam
    @param apiKey String
    @param name String
    @return Team
do
  let body = json.stringify({name: name})
  var headers = {Authorization: 'Bearer token'}      # identifier keys work in literal
  headers = set(headers, 'x-api-key', apiKey)
  let res = request.post('https://api.example.com/teams', body, headers: headers)
  let parsed = json.parse(res.body)
  Team(
    id:          unwrap(getString(parsed, 'id'), ''),
    name:        unwrap(getString(parsed, 'name'), ''),
    playerCount: 0,
  )
end
"#;

const EXAMPLE_FUNCTION_AND_TEST: &str = r#"
# Function + test — the required pattern for every public function in forai.
#
# Rules:
# - Every named public function needs a doc comment (above `def`)
# - Every public function needs at least one test block or `fai test` fails
# - Test the function in the same file, immediately after the function
# - Private helpers are covered when their callers are tested

use std.string
use std.array

# Check if an email address looks valid (contains @ and a dot after it).
def isValidEmail
    @param email String
    @return Bool
do
  let atIdx = string.indexOf(email, '@')
  if atIdx < 0
    false
  else
    let afterAt = string.substring(email, atIdx + 1, string.length(email))
    string.contains(afterAt, '.')
  end
end

test isValidEmail
it 'accepts valid email'
  assert.isTrue(isValidEmail('user@example.com'))
end
it 'rejects missing @'
  assert.isFalse(isValidEmail('notanemail'))
end
it 'rejects missing dot after @'
  assert.isFalse(isValidEmail('user@nodot'))
end
end

# Filter items from a list that match a predicate.
def filterBy
    @type T
    @param items T[]
    @param pred (T) -> Bool
    @return T[]
do
  array.filter(items, pred)
end

test filterBy
it 'keeps only matching items'
  let nums = [1, 2, 3, 4, 5]
  let evens = filterBy(nums, do with n Int n % 2 == 0 end)
  assert.equals(array.length(evens), 2)
end
end
"#;

const EXAMPLE_TYPES: &str = r#"
# Types — how to define and use struct types in forai
#
# Types are declared with `type`, constructed with named args.
# Fields are accessed with dot notation.
# Field mutation requires a `var` binding.
# Arrays: [1, 2, 3] (commas required). Optionals: Type? (use !, unwrap, or ?).

type Address
  street String
  city String
  zip String
end

type User
  id Int
  name String
  email String
  address Address?     # optional nested type
end

def makeUser
    @param id Int
    @param name String
    @param email String
    @return User
do
  User(id: id, name: name, email: email, address: null)
end

def setAddress
    @param user User, mutable
    @param street String
    @param city String
    @param zip String
    @return Void
do
  user.address = Address(street: street, city: city, zip: zip)
end

def cityOf
    @param user User
    @return String
do
  if user.address?
    user.address!.city
  else
    'unknown'
  end
end

test cityOf
it 'returns unknown when no address'
  let u = makeUser(1, 'Alice', 'a@b.com')
  assert.equals(cityOf(u), 'unknown')
end
it 'returns city when address set'
  var u = makeUser(1, 'Alice', 'a@b.com')
  setAddress(u, '123 Main St', 'Portland', '97201')
  assert.equals(cityOf(u), 'Portland')
end
end

# Built-in types cheat sheet:
# Int, Float, String, Bool, Void
# Int[]           array literal: [1, 2, 3]    length: length(arr)    access: arr[0]
# String?         check: x?     unwrap: x!    safe: unwrap(x, fallback)
# Dictionary      getString(d,'key') -> String?    getInt -> Int?    set(d,'k',v) -> Dict
"#;

const EXAMPLE_CHILDREN_COMPONENT: &str = r#"
# Custom component that accepts a do...end children block
#
# Agents often hit parse errors trying to write `Section do ... end` as a
# custom component. The trick: the parameter MUST be typed `Children`
# (which is `type def Children @return Void end` — a closure taking no
# args and returning Void). Call the closure inside your function at the
# spot where the children should render.
#
# Built-in containers (VStack, HStack, Window, ScrollView) all use this
# same mechanism — a Children param and an internal buildContainer call.

use { ViewNode, VStack, Label, background, buildContainer, childCount, cornerRadius, findByKind, fontSize, fontWeight, foreground, getChild, getProp, padding } from Forui.view
use { testMount } from Forui

# A reusable card with a title row and a body builder.
# Callers invoke it with a trailing do...end block:
#
#     Section('Profile') do
#         Label('Name').fontWeight('600')
#         Label('Jane Doe')
#     end
#
# `builder` is the Children closure; call it where the children should go.
def Section
    @param title String
    @param builder Children
    @return ViewNode
do
  let card = VStack do
      Label(title).fontSize(13).foreground('#6d6d72').fontWeight('600')
      builder()
  end
  card.padding(16).background('#ffffff').cornerRadius(12)
end

# If you need a container with no title, delegate to buildContainer directly.
# Pass your own kind name, a props dict, and the children closure — this is
# exactly how VStack/HStack are implemented in Forui.view.
def Panel
    @param children Children
    @return ViewNode
do
  buildContainer('VStack', {}, children)
end

# A page used as the test target. testMount takes a function that returns
# ViewNode, so inline usage goes inside a named def like this one.
def ProfilePage
    @return ViewNode
do
  Section('Profile') do
      Label('Jane Doe').fontWeight('700')
      Label('jane@example.com').foreground('#6d6d72')
  end
end

def PanelPage
    @return ViewNode
do
  Panel do
      Label('hi')
  end
end

test Section
it 'renders the title label and the builder-supplied children'
  let tree = testMount(ProfilePage)
  # The outer node is the VStack produced by Section.
  assert.equals(tree.kind, 'VStack')
  # Title + two builder-supplied children = 3 children total.
  assert.equals(childCount(tree), 3)
  # First child is the title label produced by Section itself.
  let title = getChild(tree, 0)
  assert.equals(unwrap(getProp(title, 'text'), ''), 'Profile')
  # The name label from the builder is also present.
  let name = findByKind(tree, 'Label')
  assert.isTrue(name?)
end
end

test Panel
it 'wraps raw children with buildContainer'
  let tree = testMount(PanelPage)
  assert.equals(tree.kind, 'VStack')
  assert.equals(childCount(tree), 1)
end
end

# Rules to remember
#   1. Parameter type must be `Children` (not `ViewNode`, not a function literal).
#   2. Call `builder()` (or whatever you named the param) at the spot in the
#      parent container where the children should appear.
#   3. Wrap with buildContainer(kind, props, children) if you want a node
#      whose children are exactly what the caller's block builds.
"#;

const EXAMPLE_UI_TESTING: &str = r#"
# UI component testing with testMount
#
# testMount(component) renders a component to a ViewNode tree without a browser.
# It calls renderSSR internally — no adapter, no signals loaded.
# Use childCount, getChild, getProp, findByKind, findByProp to inspect the tree.
# testMount is defined in Forui (forui.fai) and requires `use { testMount } from Forui`.

use { ViewNode, VStack, HStack, Label, Button, padding } from Forui.view
use { testMount, findByKind, findByProp, childCount, getChild, getProp } from Forui
use { navigate } from Forui.router

# A simple component to test:
def WelcomePage
    @return ViewNode
do
  VStack do
    Label('Welcome to My App').fontSize(24)
    Label('Please sign in to continue')
    Button('Sign In', onClick: do
      navigate('/login')
    end)
  end.padding(16)
end

test WelcomePage
it 'renders a VStack at root'
  let tree = testMount(WelcomePage)
  assert.equals(tree.kind, 'VStack')
end
it 'has 3 children'
  let tree = testMount(WelcomePage)
  assert.equals(childCount(tree), 3)
end
it 'first child is a Label with welcome text'
  let tree = testMount(WelcomePage)
  let label = getChild(tree, 0)
  assert.equals(label.kind, 'Label')
  assert.equals(unwrap(getProp(label, 'text'), ''), 'Welcome to My App')
end
it 'has a Sign In button'
  let tree = testMount(WelcomePage)
  let btn = findByKind(tree, 'Button')
  assert.isTrue(btn?)
  assert.equals(unwrap(getProp(btn!, 'text'), ''), 'Sign In')
end
it 'can find a node by prop value'
  let tree = testMount(WelcomePage)
  let label = findByProp(tree, 'text', 'Please sign in to continue')
  assert.isTrue(label?)
end
end
"#;

fn all_examples() -> &'static [ExampleDef] {
    &[
        ExampleDef {
            name: "fai.toml — single project",
            keywords: "fai.toml config project single dependencies",
            description: "Project config for a single-target app (WASM or native).",
            content: EXAMPLE_FAI_TOML_SINGLE,
        },
        ExampleDef {
            name: "fai.toml — fullstack",
            keywords: "fai.toml fullstack multi-target client server config",
            description: "Project config for a multi-target WASM client + native server.",
            content: EXAMPLE_FAI_TOML_FULLSTACK,
        },
        ExampleDef {
            name: "RPC — server",
            keywords: "rpc server remote def addRpcRoutes handleRpcRequest",
            description: "Complete RPC server: remote types, remote def functions, addRpcRoutes.",
            content: EXAMPLE_RPC_SERVER,
        },
        ExampleDef {
            name: "RPC — client",
            keywords: "rpc client Server useSignal isLoading reload setValue",
            description: "RPC client page: import from Server, useSignal with loader, mutations.",
            content: EXAMPLE_RPC_CLIENT,
        },
        ExampleDef {
            name: "HTTP + JSON",
            keywords: "http request json api parse get post headers x-api-key",
            description: "HTTP GET/POST with auth headers, JSON parsing into typed structs.",
            content: EXAMPLE_HTTP_JSON,
        },
        ExampleDef {
            name: "Function + test",
            keywords: "function def test assert basic pattern",
            description: "Required pattern: doc comment + implementation + test block.",
            content: EXAMPLE_FUNCTION_AND_TEST,
        },
        ExampleDef {
            name: "Types",
            keywords: "type struct array optional dictionary Int String Bool",
            description: "Type declarations, construction, field access, built-in types cheat sheet.",
            content: EXAMPLE_TYPES,
        },
        ExampleDef {
            name: "UI component testing",
            keywords: "ui test testMount ViewNode findByKind getProp childCount",
            description: "Test UI components with testMount, tree inspection, and assertions.",
            content: EXAMPLE_UI_TESTING,
        },
        ExampleDef {
            name: "Custom component with children block",
            keywords: "children component container custom do end block Section Panel buildContainer composition",
            description: "Write your own Section/Card-style components that accept a trailing do...end children block.",
            content: EXAMPLE_CHILDREN_COMPONENT,
        },
    ]
}

fn render_examples(query: &str) -> String {
    let examples = all_examples();

    if query.is_empty() {
        // List mode
        let mut out = String::from(
            "Available examples (call fai_examples with a keyword to get the full code):\n\n",
        );
        for ex in examples {
            out.push_str(&format!("  {}\n    {}\n\n", ex.name, ex.description));
        }
        out.push_str(
            "Keywords: fai.toml, fullstack, rpc, http, function, types, testing, ui, children",
        );
        return out;
    }

    let q = query.to_lowercase();
    let matches: Vec<&ExampleDef> = examples
        .iter()
        .filter(|ex| {
            ex.name.to_lowercase().contains(&q)
                || ex.keywords.to_lowercase().contains(&q)
                || ex.description.to_lowercase().contains(&q)
        })
        .collect();

    if matches.is_empty() {
        return format!(
            "No examples found for '{}'.\n\nAvailable keywords: fai.toml, fullstack, rpc, http, function, types, testing, ui, children\nCall fai_examples with no query to list all examples.",
            query
        );
    }

    if matches.len() == 1 {
        let ex = matches[0];
        format!("# {}\n# {}\n{}", ex.name, ex.description, ex.content)
    } else {
        let mut out = format!("Found {} examples matching '{}':\n\n", matches.len(), query);
        for ex in matches {
            out.push_str(&format!(
                "══ {} ══\n# {}\n{}\n",
                ex.name, ex.description, ex.content
            ));
        }
        out
    }
}

// ── Tool dispatch ─────────────────────────────────────────────────────

fn fai_bin() -> std::path::PathBuf {
    std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("fai"))
}

fn run_fai(args: &[&str], cwd: &str) -> (String, bool) {
    let bin = fai_bin();
    match std::process::Command::new(&bin)
        .args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
    {
        Ok(out) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stderr.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&stderr);
            }
            (text, !out.status.success())
        }
        Err(e) => (format!("failed to run fai: {}", e), true),
    }
}

fn get_str<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

fn dispatch_tool(name: &str, args: &serde_json::Value) -> (String, bool) {
    match name {
        "fai_fmt" => {
            let Some(dir) = get_str(args, "project_dir") else {
                return ("missing required parameter: project_dir".to_string(), true);
            };
            run_fai(&["fmt"], dir)
        }
        "fai_check" => {
            let Some(dir) = get_str(args, "project_dir") else {
                return ("missing required parameter: project_dir".to_string(), true);
            };
            run_fai(&["check"], dir)
        }
        "fai_test" => {
            let Some(dir) = get_str(args, "project_dir") else {
                return ("missing required parameter: project_dir".to_string(), true);
            };
            run_fai(&["test"], dir)
        }
        "fai_run" => {
            let Some(dir) = get_str(args, "project_dir") else {
                return ("missing required parameter: project_dir".to_string(), true);
            };
            run_fai(&["run"], dir)
        }
        "fai_build" => {
            let Some(dir) = get_str(args, "project_dir") else {
                return ("missing required parameter: project_dir".to_string(), true);
            };
            if let Some(target) = get_str(args, "target") {
                run_fai(&["build", target], dir)
            } else {
                run_fai(&["build"], dir)
            }
        }
        "fai_doc" => {
            let Some(dir) = get_str(args, "project_dir") else {
                return ("missing required parameter: project_dir".to_string(), true);
            };
            if let Some(query) = get_str(args, "query") {
                run_fai(&["doc", query], dir)
            } else {
                run_fai(&["doc"], dir)
            }
        }
        "fai_examples" => {
            // project_dir is required by schema but not used — examples are static.
            let query = get_str(args, "query").unwrap_or("");
            (render_examples(query), false)
        }
        "fai_new" => {
            let Some(parent) = get_str(args, "parent_dir") else {
                return ("missing required parameter: parent_dir".to_string(), true);
            };
            let Some(name) = get_str(args, "name") else {
                return ("missing required parameter: name".to_string(), true);
            };
            run_fai(&["new", name], parent)
        }
        _ => (format!("unknown tool: {}", name), true),
    }
}

// ── Message handler ───────────────────────────────────────────────────

fn handle_message(line: &str) -> Option<RpcResponse> {
    let req: RpcRequest = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            return Some(RpcResponse::err(
                serde_json::Value::Null,
                -32700,
                format!("parse error: {}", e),
            ));
        }
    };

    let id = req.id.clone().unwrap_or(serde_json::Value::Null);

    match req.method.as_str() {
        "initialize" => Some(RpcResponse::ok(
            id,
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "fai-mcp",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        )),

        "notifications/initialized" => None,

        "tools/list" => {
            let tools: Vec<serde_json::Value> = all_tools()
                .iter()
                .map(|t| {
                    let schema: serde_json::Value =
                        serde_json::from_str(t.schema).unwrap_or(serde_json::Value::Null);
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": schema,
                    })
                })
                .collect();
            Some(RpcResponse::ok(id, serde_json::json!({ "tools": tools })))
        }

        "tools/call" => {
            let params = req.params.as_ref();
            let tool_name = params
                .and_then(|p| p.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let empty = serde_json::Value::Object(Default::default());
            let tool_args = params.and_then(|p| p.get("arguments")).unwrap_or(&empty);

            if tool_name.is_empty() {
                return Some(RpcResponse::err(id, -32602, "missing tool name"));
            }

            let (text, is_error) = dispatch_tool(tool_name, tool_args);
            Some(RpcResponse::ok(
                id,
                serde_json::json!({
                    "content": [{ "type": "text", "text": text }],
                    "isError": is_error,
                }),
            ))
        }

        _ => {
            if req.id.is_none() {
                None
            } else {
                Some(RpcResponse::err(
                    id,
                    -32601,
                    format!("method not found: {}", req.method),
                ))
            }
        }
    }
}

// ── Server entry point ────────────────────────────────────────────────

pub fn cmd_mcp() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(resp) = handle_message(&line) {
            if serde_json::to_writer(&mut out, &resp).is_err() {
                break;
            }
            if out.write_all(b"\n").is_err() {
                break;
            }
            if out.flush().is_err() {
                break;
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── fai_examples ─────────────────────────────────────────────────

    #[test]
    fn test_examples_empty_query_lists_all() {
        let result = render_examples("");
        assert!(
            result.contains("Available examples"),
            "should list examples"
        );
        assert!(result.contains("fai.toml"), "should mention fai.toml");
        assert!(result.contains("RPC"), "should mention RPC");
        assert!(result.contains("HTTP"), "should mention HTTP");
        assert!(result.contains("testing"), "should mention testing");
    }

    #[test]
    fn test_examples_rpc_query_returns_server_and_client() {
        let result = render_examples("rpc");
        assert!(
            result.contains("remote def"),
            "should show remote def syntax"
        );
        assert!(
            result.contains("addRpcRoutes"),
            "should show addRpcRoutes pattern"
        );
        assert!(
            result.contains("handleRpcRequest"),
            "should show handleRpcRequest import"
        );
    }

    #[test]
    fn test_examples_fai_toml_query_returns_config() {
        let result = render_examples("fai.toml");
        assert!(result.contains("[project]"), "should show project config");
        assert!(result.contains("source_root"), "should show source_root");
    }

    #[test]
    fn test_examples_fullstack_query_returns_multi_target() {
        let result = render_examples("fullstack");
        assert!(
            result.contains("[project.client]"),
            "should show client target"
        );
        assert!(
            result.contains("[project.server]"),
            "should show server target"
        );
        assert!(
            result.contains("wasm-html"),
            "should show client target type"
        );
    }

    #[test]
    fn test_examples_http_query_returns_request_pattern() {
        let result = render_examples("http");
        assert!(result.contains("request.get"), "should show GET pattern");
        assert!(result.contains("json.parse"), "should show JSON parsing");
        assert!(result.contains("getString"), "should show getString usage");
        assert!(result.contains("x-api-key"), "should show header pattern");
    }

    #[test]
    fn test_examples_children_query_returns_children_pattern() {
        let result = render_examples("children");
        assert!(
            result.contains("@param builder Children")
                || result.contains("@param children Children"),
            "should show Children param typing"
        );
        assert!(
            result.contains("builder()") || result.contains("children()"),
            "should show calling the children closure"
        );
        assert!(
            result.contains("buildContainer"),
            "should mention buildContainer escape hatch"
        );
    }

    #[test]
    fn test_examples_ui_query_returns_testmount() {
        let result = render_examples("ui");
        assert!(result.contains("testMount"), "should show testMount");
        assert!(result.contains("findByKind"), "should show findByKind");
        assert!(result.contains("getProp"), "should show getProp");
        assert!(result.contains("childCount"), "should show childCount");
    }

    #[test]
    fn test_examples_types_query_returns_type_syntax() {
        let result = render_examples("types");
        assert!(result.contains("type "), "should show type declaration");
        assert!(result.contains("getString"), "should show dict access");
        assert!(result.contains("String?"), "should show optional type");
    }

    #[test]
    fn test_examples_unknown_query_returns_helpful_message() {
        let result = render_examples("zzz_nonexistent_xyz");
        assert!(result.contains("No examples found"), "should say not found");
        assert!(result.contains("fai.toml"), "should suggest keywords");
    }

    #[test]
    fn test_examples_all_have_nonempty_content() {
        for ex in all_examples() {
            assert!(
                !ex.content.trim().is_empty(),
                "example '{}' has empty content",
                ex.name
            );
            assert!(
                !ex.description.is_empty(),
                "example '{}' has empty description",
                ex.name
            );
            assert!(
                !ex.keywords.is_empty(),
                "example '{}' has empty keywords",
                ex.name
            );
        }
    }

    // ── handle_message — tools/list shows fai_examples ───────────────

    #[test]
    fn test_tools_list_includes_fai_examples() {
        let response = handle_message(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).unwrap();
        let json = serde_json::to_string(&response).unwrap();
        assert!(
            json.contains("fai_examples"),
            "tools/list must include fai_examples"
        );
    }

    #[test]
    fn test_initialize_no_resources_capability() {
        let response =
            handle_message(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
                .unwrap();
        let json = serde_json::to_string(&response).unwrap();
        // resources capability should be gone
        assert!(
            !json.contains("\"resources\""),
            "resources capability should be removed"
        );
        // tools capability must remain
        assert!(
            json.contains("\"tools\""),
            "tools capability must be present"
        );
    }

    #[test]
    fn test_resources_list_returns_method_not_found() {
        let response =
            handle_message(r#"{"jsonrpc":"2.0","id":1,"method":"resources/list"}"#).unwrap();
        let json = serde_json::to_string(&response).unwrap();
        assert!(
            json.contains("method not found"),
            "resources/list should be gone"
        );
    }

    #[test]
    fn test_tools_call_fai_examples_no_query() {
        // fai_examples with no query lists all examples
        let msg = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"fai_examples","arguments":{"project_dir":"/tmp"}}}"#;
        let response = handle_message(msg).unwrap();
        let json = serde_json::to_string(&response).unwrap();
        assert!(
            json.contains("Available examples"),
            "should list all examples"
        );
        assert!(!json.contains("\"isError\":true"), "should not be an error");
    }

    #[test]
    fn test_tools_call_fai_examples_with_rpc_query() {
        let msg = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"fai_examples","arguments":{"project_dir":"/tmp","query":"rpc"}}}"#;
        let response = handle_message(msg).unwrap();
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("remote def"), "should return RPC example");
        assert!(!json.contains("\"isError\":true"), "should not be an error");
    }
}
