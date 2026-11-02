//! The menu bar the app draws itself (Linux; ui/areas/menubar.slint), with
//! the app's own menus (`MenuHost`, as every context menu).
//!
//! macOS and Windows keep Slint's `MenuBar` (native there). Its menus are
//! the one definition: this module reads the `MenuBar { … }` of
//! ui/app.slint (compiled in) and builds each menu from it when it opens:
//! titles, order, separators, `if` and `for` entries, shortcuts, enabled
//! and checked states, and what an item does. The expressions those use
//! are evaluated against the window's globals (`WindowEnv`); a name this
//! module doesn't know is an error, which the tests below turn into a CI
//! failure, so neither side can change without the other.
//!
//! Shortcuts stay Slint's: the hidden `MenuBar` still matches them for the
//! whole window (i-slint-core `process_menubar_shortcuts` doesn't look at
//! its visibility).

use std::cell::RefCell;
use std::rc::Rc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use super::{Area, Changed};
use crate::AppWindow;
use crate::app::App;
use crate::kit::MenuBuilder;

/// The window's interface, whose `MenuBar` this bar draws.
const APP_SLINT: &str = include_str!("../../ui/app.slint");

/// The prefix of the menus' owners (`Menus.request`): "menubar:File".
pub const OWNER: &str = "menubar:";

#[derive(Default)]
pub struct MenuBar {
    /// What the items of the open menu do, by id ("bar:N", N the index).
    actions: RefCell<Vec<Action>>,
}

/// Where the bar goes (`ISOCAST_UI_MENU_BAR`, else the platform's).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Layout {
    /// Slint's MenuBar (macOS and Windows: native).
    Native,
    /// The app's bar under the window's title bar.
    Under,
    /// The app's bar with the window's title and buttons in it, and no
    /// title bar of the system's.
    Merged,
}

/// Linux: merge the title into the bar where the window would otherwise
/// draw its own title bar (client-side decorations: GNOME, weston). Off:
/// the bar sits under the title bar everywhere (#197, item 7, layout a).
#[cfg(target_os = "linux")]
pub const MERGE_WHERE_DECORATED: bool = false;

/// The layout asked for: `ISOCAST_UI_MENU_BAR` (`native`, `under` or
/// `merged`), else the platform's: Linux draws its own, under the title
/// bar.
pub fn layout() -> Layout {
    match std::env::var("ISOCAST_UI_MENU_BAR").as_deref() {
        Ok("native") => Layout::Native,
        Ok("under") => Layout::Under,
        Ok("merged") => Layout::Merged,
        Ok(other) => {
            isocast_diagnostics::diagnostic!("warn"; "ISOCAST_UI_MENU_BAR={other:?} is not a layout; valid values are native, under, merged (the platform's)");
            platform_layout()
        }
        Err(_) => platform_layout(),
    }
}

fn platform_layout() -> Layout {
    if cfg!(target_os = "linux") {
        Layout::Under
    } else {
        Layout::Native
    }
}

impl Area for MenuBar {
    fn connect(&self, app: &Rc<App>, window: &AppWindow) {
        let area = window.global::<crate::MenuBarArea>();
        let layout = layout();
        area.set_shown(layout != Layout::Native);
        area.set_merged(layout == Layout::Merged);
        if !app.mode.offscreen {
            let weak = window.as_weak();
            area.on_move_window(move || {
                if let Some(window) = weak.upgrade() {
                    move_window(&window);
                }
            });
            let weak = window.as_weak();
            area.on_resize_window(move |edge| {
                if let Some(window) = weak.upgrade() {
                    resize_window(&window, &edge);
                }
            });
        }
        self.show_titles(window);
    }

    fn update(&self, _app: &Rc<App>, window: &AppWindow, _changed: Changed) {
        self.show_titles(window);
    }

    fn context_menu(&self, app: &Rc<App>, owner: &str) -> Option<MenuBuilder> {
        let title = owner.strip_prefix(OWNER)?;
        let window = app.window()?;
        let env = WindowEnv(&window);
        match menu(&bar(), &env, title) {
            Ok((builder, actions)) => {
                *self.actions.borrow_mut() = actions;
                Some(builder)
            }
            Err(error) => {
                isocast_diagnostics::diagnostic!("warn"; "menu bar: {title}: {error}");
                None
            }
        }
    }

    fn menu_chosen(&self, app: &Rc<App>, owner: &str, id: &str) -> bool {
        if !owner.starts_with(OWNER) {
            return false;
        }
        let action = id
            .strip_prefix("bar:")
            .and_then(|n| n.parse::<usize>().ok())
            .and_then(|i| self.actions.borrow().get(i).cloned());
        if let (Some(action), Some(window)) = (action, app.window()) {
            run(&window, &action);
        }
        true
    }
}

impl MenuBar {
    fn show_titles(&self, window: &AppWindow) {
        let titles = match titles(&bar(), &WindowEnv(window)) {
            Ok(titles) => titles,
            Err(error) => {
                isocast_diagnostics::diagnostic!("warn"; "menu bar: {error}");
                Vec::new()
            }
        };
        let menus = window.global::<crate::Menus>();
        let now = menus.get_bar_titles();
        let same = now.row_count() == titles.len()
            && titles
                .iter()
                .enumerate()
                .all(|(i, t)| now.row_data(i).as_ref() == Some(t));
        if !same {
            menus.set_bar_titles(ModelRc::new(VecModel::from(titles)));
        }
    }
}

/// Move the window with the pointer (a press on the merged bar).
fn move_window(window: &AppWindow) {
    use slint::winit_030::WinitWindowAccessor;
    window.window().with_winit_window(|w| {
        if let Err(error) = w.drag_window() {
            isocast_diagnostics::diagnostic!("warn"; "menu bar: moving the window: {error}");
        }
    });
}

/// Resize the window from `edge` ("n", "se"…) with the pointer.
fn resize_window(window: &AppWindow, edge: &str) {
    use slint::winit_030::WinitWindowAccessor;
    use slint::winit_030::winit::window::ResizeDirection as D;
    let direction = match edge {
        "n" => D::North,
        "ne" => D::NorthEast,
        "e" => D::East,
        "se" => D::SouthEast,
        "s" => D::South,
        "sw" => D::SouthWest,
        "w" => D::West,
        _ => D::NorthWest,
    };
    window.window().with_winit_window(|w| {
        if let Err(error) = w.drag_resize_window(direction) {
            isocast_diagnostics::diagnostic!("warn"; "menu bar: resizing the window: {error}");
        }
    });
}

/// Linux, after the window's first configure: with `MERGE_WHERE_DECORATED`,
/// a window drawing its own title bar (a client-side frame makes its outer
/// size taller than its inner one) gets the merged bar instead.
#[cfg(target_os = "linux")]
pub fn window_configured(app: &AppWindow, window: &slint::winit_030::winit::window::Window) {
    use slint::winit_030::winit::platform::wayland::WindowExtWayland;
    if !MERGE_WHERE_DECORATED || layout() != Layout::Under || window.xdg_toplevel().is_none() {
        return;
    }
    let area = app.global::<crate::MenuBarArea>();
    if area.get_merged() || !window.is_decorated() {
        return;
    }
    if window.outer_size().height > window.inner_size().height {
        area.set_merged(true);
    }
}

/// ui/app.slint's `MenuBar`, read once.
fn bar() -> Rc<Element> {
    thread_local! {
        static BAR: Rc<Element> = Rc::new(parse_bar(APP_SLINT).expect("ui/app.slint's MenuBar reads (see the tests)"));
    }
    BAR.with(Rc::clone)
}

// ---------- What app.slint says ----------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Num(f64),
    /// `@name(…)` (`@keys`, `@image-url`): the name and what is between
    /// the parentheses.
    At(String, String),
    Punct(&'static str),
}

const PUNCTS: &[&str] = &[
    "=>", "&&", "||", "==", "!=", ">=", "<=", ":=", "{", "}", "(", ")", "[", "]", ";", ":", ",",
    ".", "!", "?", "+", "-", "*", "/", ">", "<",
];

fn tokens(text: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
        } else if c == '"' {
            let mut s = String::new();
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' {
                    i += 1;
                    if chars.get(i) == Some(&'{') {
                        return Err(
                            "a string with \\{…} in it isn't read here (string interpolation)"
                                .into(),
                        );
                    }
                    match chars.get(i) {
                        Some('n') => s.push('\n'),
                        Some(&other) => s.push(other),
                        None => {}
                    }
                } else {
                    s.push(chars[i]);
                }
                i += 1;
            }
            i += 1;
            out.push(Tok::Str(s));
        } else if c == '@' {
            let start = i + 1;
            i += 1;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '-') {
                i += 1;
            }
            let name: String = chars[start..i].iter().collect();
            if chars.get(i) != Some(&'(') {
                out.push(Tok::At(name, String::new()));
                continue;
            }
            i += 1;
            let mut depth = 1;
            let mut inner = String::new();
            while i < chars.len() {
                match chars[i] {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                inner.push(chars[i]);
                i += 1;
            }
            i += 1;
            out.push(Tok::At(name, inner));
        } else if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            let number: String = chars[start..i].iter().collect();
            out.push(Tok::Num(
                number.parse().map_err(|_| format!("bad number {number}"))?,
            ));
            // A unit (`px`, `ms`): not used by menus.
            if i < chars.len() && chars[i].is_alphabetic() {
                return Err(format!("{number} with a unit isn't read here"));
            }
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len()
                && (chars[i].is_alphanumeric()
                    || chars[i] == '_'
                    || (chars[i] == '-' && chars.get(i + 1).is_some_and(|n| n.is_alphabetic())))
            {
                i += 1;
            }
            out.push(Tok::Ident(chars[start..i].iter().collect()));
        } else {
            let rest: String = chars[i..chars.len().min(i + 2)].iter().collect();
            let Some(p) = PUNCTS.iter().find(|p| rest.starts_with(**p)) else {
                return Err(format!("unexpected {c:?}"));
            };
            i += p.len();
            out.push(Tok::Punct(p));
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Bar,
    Menu,
    Item,
    Separator,
}

impl Kind {
    /// The properties the bar reads; any other is an error.
    fn properties(self) -> &'static [&'static str] {
        match self {
            Kind::Bar => &["visible"],
            Kind::Menu => &["title", "enabled"],
            Kind::Item => &["title", "shortcut", "enabled", "checkable", "checked"],
            Kind::Separator => &[],
        }
    }
}

/// An element of the menu bar (`Menu`, `MenuItem`, `MenuSeparator`).
#[derive(Debug)]
struct Element {
    kind: Kind,
    /// Its properties: name and the expression, as tokens.
    props: Vec<(String, Vec<Tok>)>,
    /// `activated => { … }`: one call each.
    activated: Vec<Vec<Tok>>,
    children: Vec<Child>,
}

impl Element {
    fn prop(&self, name: &str) -> Option<&[Tok]> {
        self.props
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_slice())
    }
}

#[derive(Debug)]
struct Child {
    /// `if CONDITION:`
    condition: Option<Vec<Tok>>,
    /// `for NAME in MODEL:`
    repeat: Option<(String, Vec<Tok>)>,
    element: Element,
}

/// The `MenuBar { … }` in `text`.
fn parse_bar(text: &str) -> Result<Element, String> {
    let tokens = tokens(bar_text(text)?)?;
    let mut parser = Parser {
        tokens: &tokens,
        at: 0,
    };
    let bar = parser.element()?;
    if bar.kind != Kind::Bar {
        return Err("expected `MenuBar {`".into());
    }
    Ok(bar)
}

/// The text of the `MenuBar { … }` element in `text`, braces matched
/// past strings and comments (the rest of the file isn't read).
fn bar_text(text: &str) -> Result<&str, String> {
    let start = text
        .match_indices("MenuBar")
        .map(|(i, _)| i)
        .find(|&i| {
            let before = text[..i].chars().next_back();
            let after = text[i + "MenuBar".len()..].trim_start();
            !before.is_some_and(|c| c.is_alphanumeric() || c == '-' || c == '_')
                && after.starts_with('{')
                && !text[..i].rsplit('\n').next().unwrap_or("").contains("//")
        })
        .ok_or("no `MenuBar {` in ui/app.slint")?;
    let bytes = text.as_bytes();
    let (mut i, mut depth, mut opened) = (start, 0usize, false);
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'{' => {
                depth += 1;
                opened = true;
            }
            b'}' => {
                depth -= 1;
                if opened && depth == 0 {
                    return Ok(&text[start..=i]);
                }
            }
            _ => {}
        }
        i += 1;
    }
    Err("`MenuBar {` isn't closed".into())
}

struct Parser<'a> {
    tokens: &'a [Tok],
    at: usize,
}

impl Parser<'_> {
    fn peek(&self, ahead: usize) -> Option<&Tok> {
        self.tokens.get(self.at + ahead)
    }

    fn next(&mut self) -> Result<Tok, String> {
        let tok = self.tokens.get(self.at).cloned().ok_or("ends early")?;
        self.at += 1;
        Ok(tok)
    }

    fn expect(&mut self, punct: &'static str) -> Result<(), String> {
        match self.next()? {
            Tok::Punct(p) if p == punct => Ok(()),
            other => Err(format!("expected `{punct}`, found {other:?}")),
        }
    }

    fn ident(&mut self) -> Result<String, String> {
        match self.next()? {
            Tok::Ident(name) => Ok(name),
            other => Err(format!("expected a name, found {other:?}")),
        }
    }

    /// Tokens up to `end` at depth 0 (not included; consumed).
    fn until(&mut self, end: &'static str) -> Result<Vec<Tok>, String> {
        let mut depth = 0;
        let mut out = Vec::new();
        loop {
            let tok = self.next()?;
            match &tok {
                Tok::Punct(p) if *p == end && depth == 0 => return Ok(out),
                Tok::Punct("(" | "[" | "{") => depth += 1,
                Tok::Punct(")" | "]" | "}") => {
                    if depth == 0 {
                        return Err(format!("expected `{end}` before {tok:?}"));
                    }
                    depth -= 1
                }
                _ => {}
            }
            out.push(tok);
        }
    }

    /// `Name { … }`
    fn element(&mut self) -> Result<Element, String> {
        let name = self.ident()?;
        let kind = match name.as_str() {
            "MenuBar" => Kind::Bar,
            "Menu" => Kind::Menu,
            "MenuItem" => Kind::Item,
            "MenuSeparator" => Kind::Separator,
            other => return Err(format!("{other} isn't a menu element")),
        };
        self.expect("{")?;
        let mut element = Element {
            kind,
            props: Vec::new(),
            activated: Vec::new(),
            children: Vec::new(),
        };
        loop {
            match self.peek(0) {
                Some(Tok::Punct("}")) => {
                    self.at += 1;
                    return Ok(element);
                }
                Some(Tok::Ident(word)) if word == "if" => {
                    self.at += 1;
                    let condition = self.until(":")?;
                    element.children.push(Child {
                        condition: Some(condition),
                        repeat: None,
                        element: self.element()?,
                    });
                }
                Some(Tok::Ident(word)) if word == "for" => {
                    self.at += 1;
                    let name = self.ident()?;
                    if self.peek(0) == Some(&Tok::Punct("[")) {
                        return Err(format!("`for {name}[index]` isn't read here"));
                    }
                    if self.ident()? != "in" {
                        return Err(format!("expected `in` after `for {name}`"));
                    }
                    let model = self.until(":")?;
                    element.children.push(Child {
                        condition: None,
                        repeat: Some((name, model)),
                        element: self.element()?,
                    });
                }
                Some(Tok::Ident(_)) => match self.peek(1) {
                    Some(Tok::Punct(":")) => {
                        let name = self.ident()?;
                        if !kind.properties().contains(&name.as_str()) {
                            return Err(format!(
                                "{kind:?}'s {name} isn't read by the Linux menu bar (src/areas/menubar.rs Kind::properties)"
                            ));
                        }
                        self.at += 1;
                        let value = self.until(";")?;
                        element.props.push((name, value));
                    }
                    Some(Tok::Punct("=>")) => {
                        let name = self.ident()?;
                        if name != "activated" || kind != Kind::Item {
                            return Err(format!("{kind:?}'s callback {name} isn't read here"));
                        }
                        self.at += 1;
                        self.expect("{")?;
                        let body = self.until("}")?;
                        for call in body.split(|t| *t == Tok::Punct(";")) {
                            if !call.is_empty() {
                                element.activated.push(call.to_vec());
                            }
                        }
                    }
                    Some(Tok::Punct("{")) => element.children.push(Child {
                        condition: None,
                        repeat: None,
                        element: self.element()?,
                    }),
                    other => return Err(format!("unexpected {other:?} after a name")),
                },
                other => return Err(format!("unexpected {other:?}")),
            }
        }
    }
}

// ---------- Expressions ----------

#[derive(Debug, Clone, PartialEq)]
enum Value {
    Bool(bool),
    Str(String),
    Num(f64),
    List(Vec<Row>),
    Row(Row),
}

/// A model's row: its fields.
type Row = Vec<(String, Value)>;

impl Value {
    fn bool(&self) -> Result<bool, String> {
        match self {
            Value::Bool(b) => Ok(*b),
            other => Err(format!("{other:?} isn't true or false")),
        }
    }

    fn text(&self) -> Result<String, String> {
        match self {
            Value::Str(s) => Ok(s.clone()),
            Value::Num(n) => Ok(n.to_string()),
            other => Err(format!("{other:?} isn't text")),
        }
    }
}

/// The globals an expression reads (`Ui.studio`): `None` for a name this
/// module doesn't know.
trait Env {
    fn global(&self, global: &str, property: &str) -> Option<Value>;
}

/// Names bound by `for` (`row`, `display`).
type Scope<'a> = &'a [(String, Row)];

struct Expr<'a> {
    tokens: &'a [Tok],
    at: usize,
    env: &'a dyn Env,
    scope: Scope<'a>,
}

fn eval(tokens: &[Tok], env: &dyn Env, scope: Scope<'_>) -> Result<Value, String> {
    let mut expr = Expr {
        tokens,
        at: 0,
        env,
        scope,
    };
    let value = expr.ternary()?;
    match expr.tokens.get(expr.at) {
        None => Ok(value),
        Some(extra) => Err(format!("unexpected {extra:?}")),
    }
}

impl Expr<'_> {
    fn eat(&mut self, punct: &'static str) -> bool {
        if self.tokens.get(self.at) == Some(&Tok::Punct(punct)) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn ternary(&mut self) -> Result<Value, String> {
        let condition = self.or()?;
        if !self.eat("?") {
            return Ok(condition);
        }
        let yes = self.ternary()?;
        if !self.eat(":") {
            return Err("`?` without `:`".into());
        }
        let no = self.ternary()?;
        Ok(if condition.bool()? { yes } else { no })
    }

    fn or(&mut self) -> Result<Value, String> {
        let mut value = self.and()?;
        while self.eat("||") {
            let right = self.and()?;
            value = Value::Bool(value.bool()? || right.bool()?);
        }
        Ok(value)
    }

    fn and(&mut self) -> Result<Value, String> {
        let mut value = self.compare()?;
        while self.eat("&&") {
            let right = self.compare()?;
            value = Value::Bool(value.bool()? && right.bool()?);
        }
        Ok(value)
    }

    fn compare(&mut self) -> Result<Value, String> {
        let left = self.sum()?;
        for op in ["==", "!=", ">=", "<=", ">", "<"] {
            if self.eat(op) {
                let right = self.sum()?;
                let result = match (op, &left, &right) {
                    ("==", l, r) => l == r,
                    ("!=", l, r) => l != r,
                    (_, Value::Num(l), Value::Num(r)) => match op {
                        ">=" => l >= r,
                        "<=" => l <= r,
                        ">" => l > r,
                        _ => l < r,
                    },
                    _ => return Err(format!("{left:?} {op} {right:?}")),
                };
                return Ok(Value::Bool(result));
            }
        }
        Ok(left)
    }

    fn sum(&mut self) -> Result<Value, String> {
        let mut value = self.unary()?;
        while self.eat("+") {
            let right = self.unary()?;
            value = match (value, right) {
                (Value::Num(l), Value::Num(r)) => Value::Num(l + r),
                (l, r) => Value::Str(l.text()? + &r.text()?),
            };
        }
        Ok(value)
    }

    fn unary(&mut self) -> Result<Value, String> {
        if self.eat("!") {
            return Ok(Value::Bool(!self.unary()?.bool()?));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Value, String> {
        let tok = self
            .tokens
            .get(self.at)
            .cloned()
            .ok_or("an expression ends early")?;
        self.at += 1;
        match tok {
            Tok::Str(s) => Ok(Value::Str(s)),
            Tok::Num(n) => Ok(Value::Num(n)),
            Tok::Ident(word) if word == "true" || word == "false" => {
                Ok(Value::Bool(word == "true"))
            }
            Tok::Punct("(") => {
                let value = self.ternary()?;
                if !self.eat(")") {
                    return Err("`(` without `)`".into());
                }
                Ok(value)
            }
            Tok::Ident(first) => {
                let mut path = vec![first];
                while self.eat(".") {
                    match self.tokens.get(self.at) {
                        Some(Tok::Ident(name)) => {
                            path.push(name.clone());
                            self.at += 1;
                        }
                        other => return Err(format!("expected a name after `.`, found {other:?}")),
                    }
                }
                self.path(&path)
            }
            other => Err(format!("unexpected {other:?}")),
        }
    }

    /// `row.name`, `Ui.studio`, `LibraryArea.profiles.length`.
    fn path(&self, path: &[String]) -> Result<Value, String> {
        let dotted = path.join(".");
        let (mut value, rest) = match self.scope.iter().rev().find(|(name, _)| *name == path[0]) {
            Some((_, row)) => (Value::Row(row.clone()), &path[1..]),
            None if path.len() >= 2 => (
                self.env
                    .global(&path[0], &path[1])
                    .ok_or_else(|| format!("`{}.{}` isn't known to the Linux menu bar (src/areas/menubar.rs WindowEnv)", path[0], path[1]))?,
                &path[2..],
            ),
            None => return Err(format!("`{dotted}` isn't known to the Linux menu bar")),
        };
        for field in rest {
            value = match (value, field.as_str()) {
                (Value::List(rows), "length") => Value::Num(rows.len() as f64),
                (Value::Row(row), field) => row
                    .into_iter()
                    .find(|(name, _)| name == field)
                    .map(|(_, v)| v)
                    .ok_or_else(|| format!("`{dotted}`: no field {field}"))?,
                (other, field) => return Err(format!("`{dotted}`: {other:?} has no {field}")),
            };
        }
        Ok(value)
    }
}

/// `@keys(Control + Shift + N)` as Slint shows it on Linux:
/// "Ctrl+Shift+N" (i-slint-core `impl Display for Keys`).
fn shortcut_text(keys: &str) -> Result<String, String> {
    let (mut meta, mut control, mut alt, mut shift) = (false, false, false, false);
    let mut key = None;
    for part in keys.split('+').map(str::trim) {
        match part {
            "Meta" => meta = true,
            "Control" => control = true,
            "Alt" => alt = true,
            "Shift" => shift = true,
            // Matched either way: not shown (Slint's ignore-alt/-shift).
            "Alt?" | "Shift?" => {}
            quoted if quoted.len() >= 2 && quoted.starts_with('"') && quoted.ends_with('"') => {
                key = Some(quoted[1..quoted.len() - 1].to_uppercase())
            }
            name if !name.is_empty() => key = Some(name.to_string()),
            _ => return Err(format!("@keys({keys}) has an empty part")),
        }
    }
    let key = key.ok_or_else(|| format!("@keys({keys}) has no key"))?;
    let mut text = String::new();
    for (on, word) in [
        (meta, "Super+"),
        (control, "Ctrl+"),
        (alt, "Alt+"),
        (shift, "Shift+"),
    ] {
        if on {
            text.push_str(word);
        }
    }
    text.push_str(&key);
    Ok(text)
}

// ---------- Menus ----------

/// What an item does: a call in its `activated`, with its arguments.
#[derive(Debug, Clone, PartialEq)]
struct Action {
    calls: Vec<(String, Vec<String>)>,
}

/// The calls an item may make, as `run` makes them.
const CALLS: &[&str] = &[
    "Ui.menu",
    "Ui.quit",
    "Ui.open-dialog",
    "root.edit",
    "LibraryArea.switch-to",
];

fn action(element: &Element, env: &dyn Env, scope: Scope<'_>) -> Result<Action, String> {
    let mut calls = Vec::new();
    for call in &element.activated {
        let open = call
            .iter()
            .position(|t| *t == Tok::Punct("("))
            .ok_or_else(|| format!("{call:?} isn't a call"))?;
        if call.last() != Some(&Tok::Punct(")")) {
            return Err(format!("{call:?} isn't a call"));
        }
        let name = call[..open]
            .iter()
            .map(|t| match t {
                Tok::Ident(name) => Ok(name.as_str()),
                Tok::Punct(".") => Ok("."),
                other => Err(format!("{other:?} in a call's name")),
            })
            .collect::<Result<String, String>>()?;
        if !CALLS.contains(&name.as_str()) {
            return Err(format!(
                "`{name}(…)` isn't known to the Linux menu bar (src/areas/menubar.rs CALLS, run)"
            ));
        }
        let inner = &call[open + 1..call.len() - 1];
        let mut args = Vec::new();
        let mut depth = 0;
        let mut start = 0;
        for (i, tok) in inner.iter().enumerate() {
            match tok {
                Tok::Punct("(") => depth += 1,
                Tok::Punct(")") => depth -= 1,
                Tok::Punct(",") if depth == 0 => {
                    args.push(eval(&inner[start..i], env, scope)?.text()?);
                    start = i + 1;
                }
                _ => {}
            }
        }
        if start < inner.len() {
            args.push(eval(&inner[start..], env, scope)?.text()?);
        }
        calls.push((name, args));
    }
    Ok(Action { calls })
}

fn run(window: &AppWindow, action: &Action) {
    let arg =
        |args: &[String], i: usize| SharedString::from(args.get(i).map_or("", |s| s.as_str()));
    for (name, args) in &action.calls {
        match name.as_str() {
            "Ui.menu" => window.global::<crate::Ui>().invoke_menu(arg(args, 0)),
            "Ui.quit" => window.global::<crate::Ui>().invoke_quit(),
            "Ui.open-dialog" => window
                .global::<crate::Ui>()
                .invoke_open_dialog(arg(args, 0), arg(args, 1)),
            "root.edit" => window.invoke_edit(arg(args, 0)),
            "LibraryArea.switch-to" => window
                .global::<crate::LibraryArea>()
                .invoke_switch_to(arg(args, 0), arg(args, 1)),
            _ => {}
        }
    }
}

/// The bar's titles, in order, as they show now.
fn titles(bar: &Element, env: &dyn Env) -> Result<Vec<SharedString>, String> {
    let mut titles = Vec::new();
    each(bar, env, &[], false, &mut |element, scope| {
        if element.kind == Kind::Menu {
            titles.push(title(element, env, scope)?.into());
        }
        Ok(())
    })?;
    Ok(titles)
}

fn title(element: &Element, env: &dyn Env, scope: Scope<'_>) -> Result<String, String> {
    let tokens = element
        .prop("title")
        .ok_or("a menu or item without a title")?;
    eval(tokens, env, scope)?.text()
}

/// Each child of `parent` that shows now (its `if` true; each row of its
/// `for`), with the names it sees. `all` (the tests): every child whatever
/// its `if`, which is still read; a `for` over no rows is an error, as
/// what it shows would go unread.
fn each(
    parent: &Element,
    env: &dyn Env,
    scope: Scope<'_>,
    all: bool,
    f: &mut dyn FnMut(&Element, Scope<'_>) -> Result<(), String>,
) -> Result<(), String> {
    for child in &parent.children {
        if let Some(condition) = &child.condition
            && !eval(condition, env, scope)?.bool()?
            && !all
        {
            continue;
        }
        match &child.repeat {
            None => f(&child.element, scope)?,
            Some((name, model)) => {
                let Value::List(rows) = eval(model, env, scope)? else {
                    return Err(format!("`for {name} in …` isn't over a list"));
                };
                if all && rows.is_empty() {
                    return Err(format!(
                        "`for {name} in …` has no rows here: give the test a row of it"
                    ));
                }
                for row in rows {
                    let mut inner = scope.to_vec();
                    inner.push((name.clone(), row));
                    f(&child.element, &inner)?;
                }
            }
        }
    }
    Ok(())
}

/// The menu titled `wanted`, for `Menus.show`, and what its items do (an
/// item's id is "bar:N", N its index here).
fn menu(bar: &Element, env: &dyn Env, wanted: &str) -> Result<(MenuBuilder, Vec<Action>), String> {
    let mut found = None;
    let mut actions = Vec::new();
    each(bar, env, &[], false, &mut |element, scope| {
        if found.is_none() && element.kind == Kind::Menu && title(element, env, scope)? == wanted {
            found = Some(entries(element, env, scope, &mut actions, 0, false)?);
        }
        Ok(())
    })?;
    let builder = found.ok_or_else(|| format!("no menu {wanted:?}"))?;
    Ok((builder, actions))
}

/// A menu's entries, as Slint lists them: no separator first, last or
/// twice in a row.
fn entries(
    menu: &Element,
    env: &dyn Env,
    scope: Scope<'_>,
    actions: &mut Vec<Action>,
    depth: usize,
    all: bool,
) -> Result<MenuBuilder, String> {
    let mut builder = Some(MenuBuilder::new());
    let mut any = false;
    let mut separator = false;
    each(menu, env, scope, all, &mut |element, scope| {
        let b = builder.take().expect("the builder is put back");
        let b = match element.kind {
            Kind::Separator => {
                separator = any;
                b
            }
            Kind::Bar => return Err("a MenuBar in a menu".into()),
            Kind::Menu | Kind::Item => {
                let b = if separator { b.separator() } else { b };
                separator = false;
                any = true;
                let label = title(element, env, scope)?;
                let enabled = match element.prop("enabled") {
                    Some(tokens) => eval(tokens, env, scope)?.bool()?,
                    None => true,
                };
                if element.kind == Kind::Menu {
                    if depth > 0 {
                        return Err(format!(
                            "{label}: the app's menus open one level of submenus"
                        ));
                    }
                    let items = entries(element, env, scope, actions, depth + 1, all)?;
                    b.submenu(&label, items).with(|e| e.disabled |= !enabled)
                } else {
                    let checkable = match element.prop("checkable") {
                        Some(tokens) => eval(tokens, env, scope)?.bool()?,
                        None => false,
                    };
                    let checked = match element.prop("checked") {
                        Some(tokens) => eval(tokens, env, scope)?.bool()?,
                        None => false,
                    };
                    let hint = match element.prop("shortcut") {
                        Some([Tok::At(name, keys)]) if name == "keys" => shortcut_text(keys)?,
                        Some(other) => return Err(format!("{label}: shortcut {other:?}")),
                        None => String::new(),
                    };
                    actions.push(action(element, env, scope)?);
                    let id = format!("bar:{}", actions.len() - 1);
                    b.item(&id, &label).with(|e| {
                        e.hint = hint.into();
                        e.checked = checkable && checked;
                        e.disabled = !enabled;
                    })
                }
            }
        };
        builder = Some(b);
        Ok(())
    })?;
    Ok(builder.expect("the builder is put back"))
}

// ---------- The window's globals ----------

struct WindowEnv<'a>(&'a AppWindow);

fn rows<T: Clone + 'static>(model: ModelRc<T>, row: impl Fn(T) -> Row) -> Value {
    Value::List(model.iter().map(row).collect())
}

impl Env for WindowEnv<'_> {
    fn global(&self, global: &str, property: &str) -> Option<Value> {
        let w = self.0;
        let ui = w.global::<crate::Ui>();
        let library = w.global::<crate::LibraryArea>();
        let developer = w.global::<crate::DeveloperArea>();
        let words = w.global::<crate::PlatformText>();
        let b = Value::Bool;
        let library_row = |r: crate::LibraryRow| {
            vec![
                ("id".into(), Value::Str(r.id.into())),
                ("name".into(), Value::Str(r.name.into())),
                ("current".into(), b(r.current)),
                ("problem".into(), b(r.problem)),
            ]
        };
        Some(match (global, property) {
            ("PlatformText", "windows") => b(words.get_windows()),
            ("PlatformText", "machine") => Value::Str(words.get_machine().into()),
            ("Ui", "preview-locked") => b(ui.get_preview_locked()),
            ("Ui", "studio") => b(ui.get_studio()),
            ("Ui", "show-source-toolbar") => b(ui.get_show_source_toolbar()),
            ("Ui", "show-status-bar") => b(ui.get_show_status_bar()),
            ("Ui", "show-docks") => b(ui.get_show_docks()),
            ("Ui", "docks-locked") => b(ui.get_docks_locked()),
            ("Ui", "show-scenes") => b(ui.get_show_scenes()),
            ("Ui", "show-sources") => b(ui.get_show_sources()),
            ("Ui", "show-mixer") => b(ui.get_show_mixer()),
            ("Ui", "show-transitions") => b(ui.get_show_transitions()),
            ("Ui", "show-controls") => b(ui.get_show_controls()),
            ("Ui", "engine-remote") => b(ui.get_engine_remote()),
            ("Ui", "window-maximized") => b(ui.get_window_maximized()),
            ("Ui", "window-fullscreen") => b(ui.get_window_fullscreen()),
            ("LibraryArea", "available") => b(library.get_available()),
            ("LibraryArea", "has-profile") => b(library.get_has_profile()),
            ("LibraryArea", "has-collection") => b(library.get_has_collection()),
            ("LibraryArea", "can-remove-profile") => b(library.get_can_remove_profile()),
            ("LibraryArea", "can-remove-collection") => b(library.get_can_remove_collection()),
            ("LibraryArea", "profiles") => rows(library.get_profiles(), library_row),
            ("LibraryArea", "collections") => rows(library.get_collections(), library_row),
            ("ProjectorsArea", "displays") => {
                rows(w.global::<crate::ProjectorsArea>().get_displays(), |d| {
                    vec![
                        ("id".into(), Value::Str(d.id.into())),
                        ("name".into(), Value::Str(d.name.into())),
                    ]
                })
            }
            ("DeveloperArea", "on") => b(developer.get_on()),
            ("DeveloperArea", "overlay") => b(developer.get_overlay()),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DeveloperArea, LibraryArea, LibraryRow, PlatformText, ProjectorDisplay, Ui};

    /// A real window (Slint's testing platform, on this test's thread),
    /// its globals set the way the tests below expect, so the names are
    /// read through `WindowEnv` itself.
    fn window(windows: bool) -> AppWindow {
        thread_local! {
            static PLATFORM: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        }
        PLATFORM.with(|set| {
            if !set.replace(true) {
                i_slint_backend_testing::init_no_event_loop();
            }
        });
        let window = AppWindow::new().expect("the window is made");
        let words = window.global::<PlatformText>();
        words.set_windows(windows);
        words.set_machine("computer".into());
        let row = |id: &str, current: bool, problem: bool| LibraryRow {
            id: id.into(),
            name: format!("Name {id}").into(),
            current,
            problem,
        };
        let library = window.global::<LibraryArea>();
        let rows = || {
            ModelRc::new(VecModel::from(vec![
                row("a", true, false),
                row("b", false, true),
            ]))
        };
        library.set_profiles(rows());
        library.set_collections(rows());
        library.set_available(true);
        library.set_has_profile(true);
        library.set_has_collection(true);
        library.set_can_remove_collection(true);
        library.set_can_remove_profile(false);
        window
            .global::<crate::ProjectorsArea>()
            .set_displays(ModelRc::new(VecModel::from(vec![ProjectorDisplay {
                id: "d1".into(),
                name: "Built-in".into(),
            }])));
        let ui = window.global::<Ui>();
        ui.set_studio(true);
        ui.set_preview_locked(true);
        window
    }

    /// Knows no global: what an unknown name meets.
    struct Nothing;

    impl Env for Nothing {
        fn global(&self, _global: &str, _property: &str) -> Option<Value> {
            None
        }
    }

    /// Every menu, every `if` branch whatever its condition, every `for`
    /// with a row: each property, expression, shortcut and call is one this
    /// module reads. How many top-level menus there are.
    fn read_everything(bar: &Element, env: &dyn Env) -> Result<usize, String> {
        let mut menus = 0;
        each(bar, env, &[], true, &mut |element, scope| {
            if element.kind == Kind::Menu {
                title(element, env, scope)?;
                if let Some(tokens) = element.prop("enabled") {
                    eval(tokens, env, scope)?.bool()?;
                }
                entries(element, env, scope, &mut Vec::new(), 0, true)?;
                menus += 1;
            }
            Ok(())
        })?;
        Ok(menus)
    }

    fn listed(builder: MenuBuilder) -> Vec<String> {
        let (menu, submenus) = builder.build();
        let show = |list: &crate::MenuList| -> Vec<String> {
            list.entries
                .iter()
                .map(|e| match e.kind {
                    crate::MenuEntryKind::Separator => "--".to_string(),
                    _ => format!(
                        "{}{}{}{}{}",
                        e.label,
                        if e.hint.is_empty() {
                            String::new()
                        } else {
                            format!(" [{}]", e.hint)
                        },
                        if e.checked { " ✓" } else { "" },
                        if e.disabled { " (off)" } else { "" },
                        if e.has_submenu { " ›" } else { "" },
                    ),
                })
                .collect()
        };
        let mut out = show(&menu);
        for sub in submenus.iter() {
            out.push("»".into());
            out.extend(show(&sub));
        }
        out
    }

    /// Everything app.slint's MenuBar says, in every branch, reads through
    /// a real window's globals (`WindowEnv`); and every menu builds as it
    /// shows, on macOS' and Windows' conventions and with developer mode on
    /// and off. Changing app.slint's menus in a way the Linux bar can't
    /// follow fails here.
    #[test]
    fn every_menu_of_app_slint_builds() {
        let bar = parse_bar(APP_SLINT).expect("app.slint's MenuBar reads");
        for windows in [false, true] {
            for developer in [false, true] {
                let window = window(windows);
                window.global::<DeveloperArea>().set_on(developer);
                let env = WindowEnv(&window);
                let menus = read_everything(&bar, &env).unwrap_or_else(|e| panic!("{e}"));
                let titles = titles(&bar, &env).expect("the titles");
                assert_eq!(titles.len(), if developer { menus } else { menus - 1 });
                for title in titles {
                    let (builder, actions) =
                        menu(&bar, &env, &title).unwrap_or_else(|e| panic!("menu {title}: {e}"));
                    assert!(!actions.is_empty(), "{title} has no items");
                    let _ = listed(builder);
                }
            }
        }
    }

    /// Linux has Windows' conventions (`PlatformText.windows`: Exit,
    /// Ctrl+Y, Maximize).
    #[test]
    fn menus_follow_app_slint() {
        let bar = parse_bar(APP_SLINT).unwrap();
        let window = window(true);
        let env = WindowEnv(&window);
        let titles: Vec<String> = titles(&bar, &env)
            .unwrap()
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            titles,
            [
                "File",
                "Edit",
                "View",
                "Docks",
                "Profile",
                "Scene Collection",
                "Tools",
                "Window",
                "Help"
            ]
        );
        let (file, actions) = menu(&bar, &env, "File").unwrap();
        assert_eq!(
            listed(file),
            [
                "Show Recordings",
                "--",
                "Add Scene [Ctrl+Shift+N]",
                "Add Source… [Ctrl+Shift+A]",
                "--",
                "Settings… [Ctrl+,]",
                "--",
                "Exit [Alt+F4]",
            ]
        );
        assert_eq!(
            actions[1].calls,
            [("Ui.menu".to_string(), vec!["new_scene".to_string()])]
        );
        let (edit, actions) = menu(&bar, &env, "Edit").unwrap();
        let edit = listed(edit);
        assert_eq!(edit[0], "Undo [Ctrl+Z]");
        assert_eq!(edit[1], "Redo [Ctrl+Y]");
        assert!(edit.contains(&"Transform ›".to_string()), "{edit:?}");
        assert!(edit.contains(&"Lock Preview ✓".to_string()), "{edit:?}");
        let transform = edit.iter().position(|e| e == "»").unwrap();
        assert_eq!(edit[transform + 1], "Edit Transform… [Ctrl+E]");
        assert_eq!(
            actions[0].calls,
            [("root.edit".to_string(), vec!["undo".to_string()])]
        );
        let (view, _) = menu(&bar, &env, "View").unwrap();
        let view = listed(view);
        assert_eq!(view[0], "Studio Mode [Ctrl+Shift+S] ✓");
        assert!(
            view.contains(&"Full Screen on Built-in".to_string()),
            "{view:?}"
        );
        let (profile, actions) = menu(&bar, &env, "Profile").unwrap();
        let profile = listed(profile);
        assert_eq!(profile[3], "Remove… (off)");
        assert_eq!(&profile[profile.len() - 2..], ["Name a ✓", "Name b (off)"]);
        assert_eq!(
            actions.last().unwrap().calls,
            [(
                "LibraryArea.switch-to".to_string(),
                vec!["profile".to_string(), "b".to_string()]
            )]
        );
        let (tools, _) = menu(&bar, &env, "Tools").unwrap();
        assert_eq!(listed(tools)[1], "Set Up from This Computer…");
    }

    /// What app.slint may not say without this module learning it.
    #[test]
    fn unknown_names_and_calls_fail() {
        let env = Nothing;
        let read = |text: &str| parse_bar(text).and_then(|bar| menu(&bar, &env, "M").map(|_| ()));
        let unknown = read(
            r#"MenuBar { Menu { title: "M"; MenuItem { title: "A"; enabled: Ui.new-thing; } } }"#,
        );
        assert!(unknown.unwrap_err().contains("Ui.new-thing"));
        let call = read(
            r#"MenuBar { Menu { title: "M"; MenuItem { title: "A"; activated => { Ui.new-call(); } } } }"#,
        );
        assert!(call.unwrap_err().contains("Ui.new-call"));
        let deep = read(
            r#"MenuBar { Menu { title: "M"; Menu { title: "B"; Menu { title: "C"; MenuItem { title: "D"; } } } } }"#,
        );
        assert!(deep.unwrap_err().contains("one level"));
        let property = read(
            r#"MenuBar { Menu { title: "M"; MenuItem { title: "A"; icon: @image-url("a.svg"); } } }"#,
        );
        assert!(property.unwrap_err().contains("icon"));
        let menu_property = read(
            r#"MenuBar { Menu { title: "M"; icon: @image-url("a.svg"); MenuItem { title: "A"; } } }"#,
        );
        assert!(menu_property.unwrap_err().contains("Menu's icon"));
        let template =
            read(r#"MenuBar { Menu { title: "M"; MenuItem { title: "A \{1 + 1}"; } } }"#);
        assert!(template.unwrap_err().contains("interpolation"));
        // A false branch is read too (by the tests' walk, not when a menu
        // shows).
        let hidden = parse_bar(
            r#"MenuBar { Menu { title: "M"; MenuItem { title: "A"; } if false: MenuItem { title: "B"; enabled: Ui.new-thing; } } }"#,
        )
        .unwrap();
        assert!(menu(&hidden, &env, "M").is_ok());
        assert!(
            read_everything(&hidden, &env)
                .unwrap_err()
                .contains("Ui.new-thing")
        );
        let hidden_call = parse_bar(
            r#"MenuBar { Menu { title: "M"; MenuItem { title: "A"; } if false: MenuItem { title: "B"; activated => { Ui.new-call(); } } } }"#,
        )
        .unwrap();
        assert!(
            read_everything(&hidden_call, &env)
                .unwrap_err()
                .contains("Ui.new-call")
        );
    }

    #[test]
    fn shortcuts_read_as_slint_shows_them_on_linux() {
        for (keys, text) in [
            ("Control + Shift + N", "Ctrl+Shift+N"),
            ("Control + \",\"", "Ctrl+,"),
            ("Control + \"/\"", "Ctrl+/"),
            ("Meta + Control + F", "Super+Ctrl+F"),
            ("Alt + F4", "Alt+F4"),
            ("F11", "F11"),
        ] {
            assert_eq!(shortcut_text(keys).unwrap(), text);
        }
    }

    #[test]
    fn separators_collapse_as_slint_s() {
        let env = Nothing;
        let bar = parse_bar(
            r#"MenuBar { Menu { title: "M";
                MenuSeparator { }
                MenuItem { title: "A"; }
                MenuSeparator { }
                if false: MenuItem { title: "B"; }
                MenuSeparator { }
                MenuItem { title: "C"; }
                MenuSeparator { }
            } }"#,
        )
        .unwrap();
        let (m, _) = menu(&bar, &env, "M").unwrap();
        assert_eq!(listed(m), ["A", "--", "C"]);
    }
}
