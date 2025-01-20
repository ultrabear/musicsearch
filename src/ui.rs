use core::fmt;
use std::{fmt::Display, time::Instant};

use cursive::{
    utils::markup,
    views::{LayerPosition, LinearLayout, ListView, Panel, TextArea, TextView},
};
use rustyline::{config::Configurer, DefaultEditor};
use tantivy::{collector::TopDocs, query::QueryParser, IndexReader};

use crate::{AudioFile, HardSchema};

struct Hyperlink<H: Display, T: Display> {
    hyperlink: H,
    text: T,
}

impl<H: Display, T: Display> Hyperlink<H, T> {
    fn new(hyperlink: H, text: T) -> Self {
        Self { hyperlink, text }
    }
}

impl<H: Display, T: Display> Display for Hyperlink<H, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "\x1b]8;;{}\x1b\\{}\x1b]8;;\x1b\\",
            self.hyperlink, self.text
        )
    }
}

fn render_search(
    search: &str,
    reader: &IndexReader,
    qp: &QueryParser,
    map: &HardSchema,
    _hostname: &str,
    output: &mut ListView,
) {
    output.clear();

    if search.is_empty() {
        return;
    }

    let q = qp.parse_query_lenient(search).0;

    let search = reader.searcher();
    let top_resp = search.search(&q, &TopDocs::with_limit(20)).unwrap();

    for (_, address) in top_resp {
        let retr = AudioFile::tantivy_recall(map, &search.doc(address).unwrap());

        let s = markup::ansi::parse(format!("{retr}"));

        //let s = format!("{}", retr);

        output.add_child("", TextView::new(s));
    }
}

fn uibox(index: &IndexReader, qp: &QueryParser, map: &HardSchema, hostname: &str) {
    let mut root = cursive::termion();

    root.add_global_callback('q', |c| c.quit());

    root.add_layer(Panel::new(
        LinearLayout::vertical()
            .child(TextArea::new())
            .child(ListView::new()),
    ));

    let mut content = String::new();

    let mut runner = root.runner();

    runner.refresh();

    while runner.is_running() {
        runner.step();

        let v = runner
            .screen_mut()
            .get_mut(LayerPosition::FromBack(0))
            .unwrap();

        let layout = v
            .downcast_mut::<Panel<LinearLayout>>()
            .unwrap()
            .get_inner_mut();

        let input: &mut TextArea = layout.get_child_mut(0).unwrap().downcast_mut().unwrap();

        if input.get_content() != content {
            content = input.get_content().to_owned();

            let output: &mut ListView = layout.get_child_mut(1).unwrap().downcast_mut().unwrap();

            render_search(&content, index, qp, map, hostname, output);
            runner.refresh();
        }
    }
}

/// All fields that a UI Requires
pub struct UIReq<'a> {
    pub index: &'a IndexReader,
    pub qp: &'a QueryParser,
    pub map: &'a HardSchema,
    pub hostname: &'a str,
}

/// An abstraction over user interface implementations
pub trait UISpawner {
    fn spawn_ui(&self, ui: UIReq<'_>);
}

pub struct CursiveUI;

impl UISpawner for CursiveUI {
    fn spawn_ui(&self, ui: UIReq<'_>) {
        uibox(ui.index, ui.qp, ui.map, ui.hostname);
    }
}

pub struct RustylineUI;

impl UISpawner for RustylineUI {
    fn spawn_ui(
        &self,
        UIReq {
            index: reader,
            qp,
            map,
            hostname,
        }: UIReq<'_>,
    ) {
        let mut editor = DefaultEditor::new().unwrap();
        editor.set_auto_add_history(true);
        editor.set_completion_type(rustyline::CompletionType::List);


        while let Ok(line) = editor.readline("> ") {
            let q = qp.parse_query_lenient(&line).0;

            let start = Instant::now();

            let search = reader.searcher();
            let top_resp = search.search(&q, &TopDocs::with_limit(15)).unwrap();

            for (_, address) in top_resp.into_iter().rev() {
                let retr = AudioFile::tantivy_recall(map, &search.doc(address).unwrap());

                println!(
                    "{}",
                    Hyperlink::new(format_args!("file://{hostname}{}", retr.file_path), &retr)
                );
            }

            if !line.is_empty() {
                println!("searched in {:?}", start.elapsed());
            }
        }
    }
}
