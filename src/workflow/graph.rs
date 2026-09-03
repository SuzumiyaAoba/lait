//! `lait graph`: renders a workflow's control-flow structure (step
//! transitions, `when`/`switch` branches, `parallel` fan-out/fan-in,
//! `loop`/`for_each` bodies) as a Mermaid or DOT graph — see
//! docs/usage/ja/workflow.md. Building (`build`) walks the step tree once
//! into a format-neutral [`GraphModel`]; `render_mermaid`/`render_dot` turn
//! that into text. A `workflow:` node is rendered as a single node naming
//! the sub-workflow file rather than expanded in place, so this module never
//! has to load or cycle-check another file — inspect a sub-workflow with its
//! own `lait graph <path>` call.

use std::collections::HashSet;

use anyhow::Result;

use super::{FlowStep, NodeDefinition, NodeMap, Router, WorkflowFile};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GraphFormat {
    Mermaid,
    Dot,
}

#[derive(Clone, Copy)]
enum NodeShape {
    /// A `use:` node (rectangle in both formats).
    Action,
    /// A `switch`/`loop`/`for_each`/`parallel` control node (diamond).
    Decision,
    /// A `stop`/`break` terminal marker (rounded/stadium shape).
    Terminal,
}

struct GraphNode {
    id: String,
    label: String,
    shape: NodeShape,
}

struct GraphEdge {
    from: String,
    to: String,
    label: Option<String>,
}

struct Subgraph {
    title: String,
    node_ids: Vec<String>,
}

/// The format-neutral result of walking a workflow's step tree, turned into
/// text by `render_mermaid`/`render_dot`. Built once by `build`, then handed
/// to whichever renderer `--format` asked for — so a bug in the walk shows up
/// identically in both outputs instead of being reimplemented (and
/// potentially diverging) per format.
struct GraphModel {
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    subgraphs: Vec<Subgraph>,
}

/// Builds `wf`'s [`GraphModel`] and renders it in `format`.
pub(crate) fn render(wf: &WorkflowFile, format: GraphFormat) -> Result<String> {
    let model = build(wf)?;
    Ok(match format {
        GraphFormat::Mermaid => render_mermaid(&model),
        GraphFormat::Dot => render_dot(&model),
    })
}

fn build(wf: &WorkflowFile) -> Result<GraphModel> {
    let mut builder = GraphBuilder::default();
    let start = builder.add_node("start".to_owned(), NodeShape::Terminal);
    let (entry, exits) = render_chain(&wf.steps, &wf.nodes, &mut builder);
    if let Some(entry) = entry {
        builder.add_edge(&start, &entry, None);
    }
    let end = builder.add_node("end".to_owned(), NodeShape::Terminal);
    for exit in exits {
        builder.add_edge(&exit, &end, None);
    }
    Ok(builder.finish())
}

#[derive(Default)]
struct GraphBuilder {
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    subgraphs: Vec<Subgraph>,
    claimed: HashSet<String>,
    counter: usize,
}

impl GraphBuilder {
    fn add_node(&mut self, label: String, shape: NodeShape) -> String {
        self.counter += 1;
        let id = format!("n{}", self.counter);
        self.nodes.push(GraphNode {
            id: id.clone(),
            label,
            shape,
        });
        id
    }

    fn add_edge(&mut self, from: &str, to: &str, label: Option<String>) {
        self.edges.push(GraphEdge {
            from: from.to_owned(),
            to: to.to_owned(),
            label,
        });
    }

    /// Groups every node added since `since` (and not already claimed by a
    /// more deeply nested subgraph — see the module doc) into one subgraph
    /// titled `title`. Called after rendering a `loop`/`for_each` body, or a
    /// `parallel` branch, so the diagram visually groups a repeated/
    /// concurrent body the way `docs/usage/ja/workflow.md` describes it.
    fn add_subgraph(&mut self, title: String, since: usize) {
        let member_ids: Vec<String> = self.nodes[since..]
            .iter()
            .map(|node| node.id.clone())
            .filter(|id| !self.claimed.contains(id))
            .collect();
        if member_ids.is_empty() {
            return;
        }
        for id in &member_ids {
            self.claimed.insert(id.clone());
        }
        self.subgraphs.push(Subgraph {
            title,
            node_ids: member_ids,
        });
    }

    fn finish(self) -> GraphModel {
        GraphModel {
            nodes: self.nodes,
            edges: self.edges,
            subgraphs: self.subgraphs,
        }
    }
}

/// Renders one step list (a workflow's top-level `steps`, a `switch` case's/
/// `else`'s/`parallel` branch's/`loop`'s/`for_each`'s own `steps`) into
/// `builder`, wiring each step to the next in sequence. Returns this list's
/// own entry node (the first step's, or `None` for an empty list) and its
/// exit nodes — normally just the last step's, but more than one when the
/// last step is itself a `switch`/`parallel` router (each of *its* branches'
/// own exits becomes an exit of this whole list, so whatever follows
/// connects from all of them).
fn render_chain(
    steps: &[FlowStep],
    nodes: &NodeMap,
    builder: &mut GraphBuilder,
) -> (Option<String>, Vec<String>) {
    let mut entry: Option<String> = None;
    let mut prev_exits: Vec<String> = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        let label = step.label_or(index + 1);
        let (step_entry, step_exits) = render_step(step, &label, nodes, builder);
        if let Some(step_entry) = &step_entry {
            if entry.is_none() {
                entry = Some(step_entry.clone());
            }
            for prev in &prev_exits {
                builder.add_edge(prev, step_entry, None);
            }
        }
        prev_exits = step_exits;
    }
    (entry, prev_exits)
}

fn render_step(
    step: &FlowStep,
    label: &str,
    nodes: &NodeMap,
    builder: &mut GraphBuilder,
) -> (Option<String>, Vec<String>) {
    if let Some(router) = step.router() {
        return render_router(router, label, nodes, builder);
    }

    match &step.r#use {
        Some(node_id) => {
            // Guaranteed by `validate::validate_steps` before a workflow is
            // ever run or graphed (see the same lookup in
            // `dryrun::print_step`/`execute_step`'s runtime version).
            let node = nodes
                .get(node_id)
                .expect("validate_steps guarantees 'use' resolves in 'nodes'");
            let mut node_label = format!("[{label}]\ntype: {}", node.type_name());
            if let NodeDefinition::Workflow(workflow_node) = node {
                node_label.push_str(&format!("\n{}", workflow_node.workflow.display()));
            }
            if let Some(when) = &step.when {
                node_label.push_str(&format!("\nwhen: {when}"));
            }
            let id = builder.add_node(node_label, NodeShape::Action);
            if let Some(on_error) = &step.on_error {
                let error_label = format!("{label}: on_error");
                let since = builder.nodes.len();
                let (on_error_entry, _) = render_chain(&on_error.steps, nodes, builder);
                if let Some(on_error_entry) = on_error_entry {
                    builder.add_edge(&id, &on_error_entry, Some("on_error".to_owned()));
                }
                builder.add_subgraph(error_label, since);
            }
            let mut exits = vec![id.clone()];
            if step.stop == Some(true) {
                let stop_id = builder.add_node("stop".to_owned(), NodeShape::Terminal);
                builder.add_edge(&id, &stop_id, None);
                exits = Vec::new();
            } else if step.r#break == Some(true) {
                let break_id = builder.add_node("break".to_owned(), NodeShape::Terminal);
                builder.add_edge(&id, &break_id, None);
                exits = Vec::new();
            }
            (Some(id), exits)
        }
        None => {
            // A standalone `stop`/`break` (no `use`, no router).
            let kind = if step.stop == Some(true) {
                "stop"
            } else {
                "break"
            };
            let id = builder.add_node(kind.to_owned(), NodeShape::Terminal);
            (Some(id), Vec::new())
        }
    }
}

fn render_router(
    router: Router<'_>,
    label: &str,
    nodes: &NodeMap,
    builder: &mut GraphBuilder,
) -> (Option<String>, Vec<String>) {
    match router {
        Router::Switch(switch) => {
            let router_id = builder.add_node(format!("[{label}]\nswitch"), NodeShape::Decision);
            let mut exits = Vec::new();
            for (index, case) in switch.cases.iter().enumerate() {
                let case_label = case
                    .id
                    .clone()
                    .unwrap_or_else(|| format!("case-{}", index + 1));
                let (case_entry, case_exits) = render_chain(&case.steps, nodes, builder);
                if let Some(case_entry) = case_entry {
                    builder.add_edge(
                        &router_id,
                        &case_entry,
                        Some(format!("{case_label}: {}", case.when)),
                    );
                }
                exits.extend(case_exits);
            }
            if let Some(else_steps) = &switch.else_steps {
                let (else_entry, else_exits) = render_chain(else_steps, nodes, builder);
                if let Some(else_entry) = else_entry {
                    builder.add_edge(&router_id, &else_entry, Some("else".to_owned()));
                }
                exits.extend(else_exits);
            }
            (Some(router_id), exits)
        }
        Router::Parallel(parallel) => {
            let fork_id = builder.add_node(format!("[{label}]\nparallel"), NodeShape::Decision);
            let join_label = match &parallel.join {
                Some(filter) => format!("join\n{filter}"),
                None => "join".to_owned(),
            };
            let join_id = builder.add_node(join_label, NodeShape::Decision);
            for (index, branch) in parallel.branches.iter().enumerate() {
                let since = builder.nodes.len();
                let (branch_entry, branch_exits) = render_chain(&branch.steps, nodes, builder);
                match branch_entry {
                    Some(branch_entry) => {
                        builder.add_edge(&fork_id, &branch_entry, None);
                        for exit in branch_exits {
                            builder.add_edge(&exit, &join_id, None);
                        }
                    }
                    None => builder.add_edge(&fork_id, &join_id, None),
                }
                builder.add_subgraph(format!("branch '{}'", branch.label(index)), since);
            }
            (Some(fork_id), vec![join_id])
        }
        Router::Loop(loop_def) => {
            let condition = match (&loop_def.r#while, &loop_def.until) {
                (Some(cond), _) => format!("while {cond}"),
                (None, Some(cond)) => format!("until {cond}"),
                (None, None) => "(no condition)".to_owned(),
            };
            let max_iterations = loop_def
                .max_iterations
                .map_or_else(|| "?".to_owned(), |n| n.to_string());
            let loop_id = builder.add_node(
                format!("[{label}]\nloop: {condition}\nmax_iterations: {max_iterations}"),
                NodeShape::Decision,
            );
            let since = builder.nodes.len();
            let (body_entry, body_exits) = render_chain(&loop_def.steps, nodes, builder);
            if let Some(body_entry) = body_entry {
                builder.add_edge(&loop_id, &body_entry, Some("iterate".to_owned()));
                for exit in body_exits {
                    builder.add_edge(&exit, &loop_id, Some("next iteration".to_owned()));
                }
            }
            builder.add_subgraph(format!("[{label}] loop body"), since);
            (Some(loop_id.clone()), vec![loop_id])
        }
        Router::ForEach(for_each) => {
            let mut node_label = format!("[{label}]\nfor_each: {}", for_each.items);
            if let Some(max_concurrency) = for_each.max_concurrency {
                node_label.push_str(&format!("\nmax_concurrency: {max_concurrency}"));
            }
            let for_each_id = builder.add_node(node_label, NodeShape::Decision);
            let since = builder.nodes.len();
            let (body_entry, body_exits) = render_chain(&for_each.steps, nodes, builder);
            if let Some(body_entry) = body_entry {
                builder.add_edge(&for_each_id, &body_entry, Some("per item".to_owned()));
                for exit in body_exits {
                    builder.add_edge(&exit, &for_each_id, Some("next item".to_owned()));
                }
            }
            builder.add_subgraph(format!("[{label}] for_each body"), since);
            (Some(for_each_id.clone()), vec![for_each_id])
        }
    }
}

fn render_mermaid(model: &GraphModel) -> String {
    let mut out = String::from("flowchart TD\n");
    for node in &model.nodes {
        let label = mermaid_escape(&node.label);
        let rendered = match node.shape {
            NodeShape::Action => format!("    {}[\"{label}\"]\n", node.id),
            NodeShape::Decision => format!("    {}{{\"{label}\"}}\n", node.id),
            NodeShape::Terminal => format!("    {}((\"{label}\"))\n", node.id),
        };
        out.push_str(&rendered);
    }
    for (index, subgraph) in model.subgraphs.iter().enumerate() {
        // Mermaid's subgraph grammar is `subgraph id [title]` — a bare
        // quoted title with no id is accepted by some renderers and not
        // others, so give every subgraph its own synthetic id the same way
        // every node gets one.
        out.push_str(&format!(
            "    subgraph sg{index}[\"{}\"]\n",
            mermaid_escape(&subgraph.title)
        ));
        for id in &subgraph.node_ids {
            out.push_str(&format!("        {id}\n"));
        }
        out.push_str("    end\n");
    }
    for edge in &model.edges {
        match &edge.label {
            Some(label) => out.push_str(&format!(
                "    {} -->|\"{}\"| {}\n",
                edge.from,
                mermaid_escape(label),
                edge.to
            )),
            None => out.push_str(&format!("    {} --> {}\n", edge.from, edge.to)),
        }
    }
    out
}

fn render_dot(model: &GraphModel) -> String {
    let mut out = String::from("digraph workflow {\n    rankdir=TB;\n");
    for node in &model.nodes {
        let label = dot_escape(&node.label);
        let shape = match node.shape {
            NodeShape::Action => "box",
            NodeShape::Decision => "diamond",
            NodeShape::Terminal => "ellipse",
        };
        out.push_str(&format!(
            "    {} [shape={shape}, label=\"{label}\"];\n",
            node.id
        ));
    }
    for (index, subgraph) in model.subgraphs.iter().enumerate() {
        out.push_str(&format!("    subgraph cluster_{index} {{\n"));
        out.push_str(&format!(
            "        label=\"{}\";\n",
            dot_escape(&subgraph.title)
        ));
        for id in &subgraph.node_ids {
            out.push_str(&format!("        {id};\n"));
        }
        out.push_str("    }\n");
    }
    for edge in &model.edges {
        match &edge.label {
            Some(label) => out.push_str(&format!(
                "    {} -> {} [label=\"{}\"];\n",
                edge.from,
                edge.to,
                dot_escape(label)
            )),
            None => out.push_str(&format!("    {} -> {};\n", edge.from, edge.to)),
        }
    }
    out.push_str("}\n");
    out
}

/// Mermaid parses node/edge-label text inside `"..."` as its own tiny
/// grammar; a literal `"` there ends the label early and a bare newline
/// breaks the line-oriented syntax entirely (a `when`/`switch`/jq
/// expression's own quotes are the case this guards against — see the
/// module doc). `<br/>` is Mermaid's own line break inside a quoted label.
fn mermaid_escape(text: &str) -> String {
    text.replace('"', "'").replace('\n', "<br/>")
}

/// DOT's quoted-string escaping: `"` and `\` are backslash-escaped, and `\l`
/// left-aligns each line instead of centering it (more readable for a
/// multi-line node/edge label than DOT's default `\n` centering).
fn dot_escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\l")
}
