//! Platform-independent accessibility tree walk with node, depth and time caps.
//!
//! Each backend (UI Automation, AXUIElement) wraps its element type in [`AxNode`]; the walk,
//! the caps and the password rule live here so they are unit-tested with fake trees.

use ssc_core::types::{Rect, ScreenNode};
use std::time::{Duration, Instant};

/// One accessibility element. Implementations do the platform calls lazily: the walker calls
/// [`AxNode::is_password`] first and never calls [`AxNode::text`] or [`AxNode::children`] on a
/// password field.
pub trait AxNode: Sized {
    /// True for `IsPassword` (UIA) / `AXSecureTextField` (AX) elements.
    fn is_password(&self) -> bool;
    /// Role name for [`ScreenNode::role`], e.g. "Edit" or "AXStaticText".
    fn role(&self) -> String;
    /// Visible text (name, value, text pattern). `None` or blank when the element has none.
    fn text(&self) -> Option<String>;
    fn bounds(&self) -> Option<Rect>;
    /// Up to `max` children in reading order.
    fn children(&self, max: usize) -> Vec<Self>;
}

#[derive(Debug, Clone, Copy)]
pub struct WalkLimits {
    /// Max elements visited (text-bearing or not).
    pub max_nodes: usize,
    pub max_depth: usize,
    /// Wall-clock budget for the whole walk.
    pub budget: Duration,
    /// Per-node text cap in bytes (huge documents are cut, on a char boundary).
    pub max_text_bytes: usize,
}

impl Default for WalkLimits {
    fn default() -> Self {
        Self { max_nodes: 2_000, max_depth: 64, budget: Duration::from_millis(50), max_text_bytes: 4_096 }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct WalkOutput {
    /// Text-bearing nodes in reading (pre-order) order. Never contains password fields.
    pub nodes: Vec<ScreenNode>,
    /// Bounds of password fields seen, so OCR can mask them.
    pub password_regions: Vec<Rect>,
    pub visited: usize,
    pub hit_node_cap: bool,
    pub hit_time_cap: bool,
}

/// Pre-order walk from `root` within `limits`. Stops early (keeping what it has) when a cap is hit.
pub fn walk<N: AxNode>(root: N, limits: &WalkLimits) -> WalkOutput {
    let deadline = Instant::now() + limits.budget;
    let mut out = WalkOutput::default();
    // Stack of (node, depth); children are pushed reversed so they pop in reading order.
    let mut stack = vec![(root, 0usize)];
    while let Some((node, depth)) = stack.pop() {
        if out.visited >= limits.max_nodes {
            out.hit_node_cap = true;
            break;
        }
        if Instant::now() >= deadline {
            out.hit_time_cap = true;
            break;
        }
        out.visited += 1;

        if node.is_password() {
            // Never read a password field or anything under it. Keep only its bounds.
            if let Some(b) = node.bounds() {
                out.password_regions.push(b);
            }
            continue;
        }

        if let Some(text) = node.text().map(|t| clean_text(&t, limits.max_text_bytes)).filter(|t| !t.is_empty()) {
            // UIA and AX often repeat a parent's name on its only child; keep one copy.
            let dup = out.nodes.last().is_some_and(|last| last.text == text);
            if !dup {
                out.nodes.push(ScreenNode { role: node.role(), text, bounds: node.bounds() });
            }
        }

        if depth < limits.max_depth {
            let remaining = limits.max_nodes.saturating_sub(out.visited + stack.len());
            if remaining > 0 {
                let mut kids = node.children(remaining);
                kids.truncate(remaining);
                stack.extend(kids.into_iter().rev().map(|k| (k, depth + 1)));
            } else {
                out.hit_node_cap = true;
            }
        }
    }
    out
}

/// Trim, drop blank lines, collapse runs of spaces/tabs, cap the size.
fn clean_text(s: &str, max_bytes: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max_bytes));
    for line in s.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&line);
        if out.len() >= max_bytes {
            break;
        }
    }
    if out.len() > max_bytes {
        let cut = (0..=max_bytes).rev().find(|&i| out.is_char_boundary(i)).unwrap_or(0);
        out.truncate(cut);
    }
    out
}

/// In-memory tree for tests and benches: records every text read so tests can prove that
/// password fields are never touched.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Debug, Clone)]
    pub struct FakeTree {
        pub role: String,
        pub text: Option<String>,
        pub password: bool,
        pub bounds: Option<Rect>,
        pub children: Vec<FakeTree>,
        /// Sleep this long when the node's text is read (simulates a slow app).
        pub delay: Duration,
    }

    impl FakeTree {
        pub fn new(role: &str, text: &str) -> Self {
            Self {
                role: role.into(),
                text: (!text.is_empty()).then(|| text.into()),
                password: false,
                bounds: None,
                children: vec![],
                delay: Duration::ZERO,
            }
        }
        pub fn password(text: &str) -> Self {
            Self { password: true, bounds: Some(Rect { x: 10, y: 20, w: 200, h: 24 }), ..Self::new("Edit", text) }
        }
        pub fn with(mut self, children: Vec<FakeTree>) -> Self {
            self.children = children;
            self
        }
        /// A window shaped like a chat/meeting app: `rows` messages of a few elements each.
        pub fn fixture(rows: usize) -> Self {
            let msgs = (0..rows)
                .map(|i| {
                    FakeTree::new("Group", "").with(vec![
                        FakeTree::new("Text", &format!("Speaker {}", i % 5)),
                        FakeTree::new("Text", &format!("Message {i}: what is the price for {i} seats?")),
                        FakeTree::new("Button", "Reply"),
                    ])
                })
                .collect();
            FakeTree::new("Window", "Zoom Meeting").with(vec![
                FakeTree::new("Pane", "").with(msgs),
                FakeTree::new("Pane", "Login")
                    .with(vec![FakeTree::new("Edit", "user@example.com"), FakeTree::password("hunter2")]),
            ])
        }
    }

    /// Cursor into a [`FakeTree`] that logs reads.
    #[derive(Clone)]
    pub struct FakeNode<'a> {
        pub tree: &'a FakeTree,
        pub reads: Rc<RefCell<Vec<String>>>,
    }

    impl<'a> FakeNode<'a> {
        pub fn root(tree: &'a FakeTree) -> Self {
            Self { tree, reads: Rc::default() }
        }
    }

    impl AxNode for FakeNode<'_> {
        fn is_password(&self) -> bool {
            self.tree.password
        }
        fn role(&self) -> String {
            self.tree.role.clone()
        }
        fn text(&self) -> Option<String> {
            if !self.tree.delay.is_zero() {
                std::thread::sleep(self.tree.delay);
            }
            if let Some(t) = &self.tree.text {
                self.reads.borrow_mut().push(t.clone());
            }
            self.tree.text.clone()
        }
        fn bounds(&self) -> Option<Rect> {
            self.tree.bounds
        }
        fn children(&self, max: usize) -> Vec<Self> {
            self.tree.children.iter().take(max).map(|c| FakeNode { tree: c, reads: self.reads.clone() }).collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;

    fn texts(o: &WalkOutput) -> Vec<&str> {
        o.nodes.iter().map(|n| n.text.as_str()).collect()
    }

    #[test]
    fn reading_order_and_roles() {
        let t = FakeTree::new("Window", "Title").with(vec![
            FakeTree::new("Text", "a").with(vec![FakeTree::new("Text", "a1")]),
            FakeTree::new("Button", "b"),
        ]);
        let o = walk(FakeNode::root(&t), &WalkLimits::default());
        assert_eq!(texts(&o), vec!["Title", "a", "a1", "b"]);
        assert_eq!(o.nodes[3].role, "Button");
        assert_eq!(o.visited, 4);
        assert!(!o.hit_node_cap && !o.hit_time_cap);
    }

    #[test]
    fn password_fields_are_never_read() {
        let secret = "hunter2-secret";
        let t = FakeTree::new("Window", "Sign in").with(vec![
            FakeTree::new("Edit", "alice@example.com"),
            // A password field with a child that also carries the secret: neither is read.
            FakeTree::password(secret).with(vec![FakeTree::new("Text", secret)]),
            FakeTree::new("Button", "Log in"),
        ]);
        let root = FakeNode::root(&t);
        let reads = root.reads.clone();
        let o = walk(root, &WalkLimits::default());
        assert_eq!(texts(&o), vec!["Sign in", "alice@example.com", "Log in"]);
        assert!(!reads.borrow().iter().any(|r| r.contains(secret)), "password text was read: {:?}", reads.borrow());
        assert!(!o.nodes.iter().any(|n| n.text.contains(secret)));
        assert_eq!(o.password_regions, vec![Rect { x: 10, y: 20, w: 200, h: 24 }]);
    }

    #[test]
    fn node_cap_stops_walk() {
        let o = walk(FakeNode::root(&FakeTree::fixture(1000)), &WalkLimits { max_nodes: 50, ..Default::default() });
        assert!(o.hit_node_cap);
        assert!(o.visited <= 50);
        assert!(!o.nodes.is_empty());
    }

    #[test]
    fn depth_cap() {
        let mut t = FakeTree::new("Text", "leaf");
        for i in 0..10 {
            t = FakeTree::new("Group", &format!("d{i}")).with(vec![t]);
        }
        let o = walk(FakeNode::root(&t), &WalkLimits { max_depth: 2, ..Default::default() });
        assert_eq!(texts(&o), vec!["d9", "d8", "d7"]);
    }

    #[test]
    fn time_cap_stops_walk() {
        let slow = |i: usize| FakeTree { delay: Duration::from_millis(5), ..FakeTree::new("Text", &format!("t{i}")) };
        let t = FakeTree::new("Window", "w").with((0..100).map(slow).collect());
        let start = Instant::now();
        let o = walk(FakeNode::root(&t), &WalkLimits { budget: Duration::from_millis(30), ..Default::default() });
        let took = start.elapsed();
        assert!(o.hit_time_cap);
        assert!(o.nodes.len() < 100);
        // One node may overrun the deadline, never the whole tree.
        assert!(took < Duration::from_millis(30 + 5 * 3), "took {took:?}");
    }

    #[test]
    fn text_is_cleaned_capped_and_deduped() {
        let long = "x".repeat(10_000);
        let t = FakeTree::new("Window", "  Hello   world \n\n  next  ").with(vec![
            FakeTree::new("Group", "Send").with(vec![FakeTree::new("Text", "Send")]),
            FakeTree::new("Document", &long),
            FakeTree::new("Text", "   "),
        ]);
        let o = walk(FakeNode::root(&t), &WalkLimits { max_text_bytes: 100, ..Default::default() });
        assert_eq!(o.nodes[0].text, "Hello world\nnext");
        assert_eq!(o.nodes[1].text, "Send");
        assert_eq!(o.nodes[2].text.len(), 100);
        assert_eq!(o.nodes.len(), 3);
    }
}
