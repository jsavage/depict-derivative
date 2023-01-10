use std::borrow::Cow;
use std::cell::Cell;
use std::io::{self};
use std::panic::catch_unwind;

use depict::graph_drawing::error::{Error, OrErrExt, Kind};
use depict::graph_drawing::eval::{Val, Body};
use depict::graph_drawing::frontend::log::Record;
use depict::graph_drawing::index::{VerticalRank, OriginalHorizontalRank, LocSol, HopSol};
use depict::graph_drawing::frontend::dom::{draw, Drawing, Label, Node};
use depict::graph_drawing::frontend::dioxus::{render, as_data_svg};

use anyhow;

use dioxus::prelude::*;
use dioxus_desktop::{self, Config, WindowBuilder};

use futures::StreamExt;

// use dioxus_desktop::tao::dpi::{LogicalSize};
// use dioxus_desktop::use_window;
use tao::dpi::LogicalSize;

use color_spantrace::colorize;

use indoc::indoc;

use tracing::{instrument, event, Level};
use tracing_error::{ExtractSpanTrace};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

const PLACEHOLDER: &str = indoc!("
k [ - s b ]
- c s
");

pub struct AppProps {
}

pub fn render_one<P>(cx: Scope<P>, record: Record) -> Option<VNode> {
    match record {
        Record::String{name, ty, names, val} => {
            let ty = ty.unwrap_or("None".into());
            let classes = names.iter().map(|n| format!("highlight_{n}")).collect::<Vec<_>>().join(" ");
            // let key = format!("debug_{ty}_{name}");
            // eprintln!("KEY: {key}");
            cx.render(rsx!{
                div {
                    // key: "debug_{ty}_{name}",
                    class: "{classes}",
                    name.map(|name| rsx!{
                        div {
                            style: "font-weight: 700;",
                            "{name}"
                        }
                    }),
                    div {
                        style: "white-space: pre; margin-left: 10px;",
                        "{val}"
                    }
                }
            })
        },
        _ => cx.render(rsx!{""}),
    }
}

fn render_many<P>(cx: Scope<P>, record: Record) -> Option<VNode> {
    match record {
        Record::String{..} => render_one(cx, record),
        Record::Group{name, ty, names, val} => {
            let classes = names.iter().map(|n| format!("highlight_{n}")).collect::<Vec<_>>().join(" ");
            let ty2 = ty.clone().unwrap_or("None".into());
            // let key = format!("debug_{ty2}_{name}");
            // eprintln!("KEY: {key}");
            cx.render(rsx!{
                div {
                    style: "margin-right: 20px;",
                    // key: "debug_{ty2}_{name}",
                    class: "{classes}",
                    details {
                        style: "padding-left: 4px;",
                        open: "true",
                        summary {
                            style: "white-space: nowrap;",
                            name.map(|name| rsx!{
                                span {
                                    "{name}: "
                                }
                            }),
                            ty.map(|ty| rsx!{
                                span {
                                    style: "font-weight: 700;",
                                    "{ty}",
                                }
                            }),
                        }
                        div {
                            style: "white-space: pre; margin-left: 10px; border-left: 1px gray solid;",
                            val.into_iter().map(|r| render_many(cx, r))
                        }
                    }
                }
            })
        },
    }
}

pub fn render_logs<P>(cx: Scope<P>, drawing: Drawing) -> Option<VNode> {
    let logs = drawing.logs;
    cx.render(rsx!{
        logs.into_iter().map(|r| render_many(cx, r))
    })
}

pub fn parse_highlights<'s>(data: &'s str) -> Result<Val<Cow<'s, str>>, Error> {
    use depict::parser::{Parser, Token};
    use depict::graph_drawing::eval::{eval, index, resolve};
    use logos::Logos;
    use std::collections::HashMap;
    use tracing_error::InstrumentResult;
    
    if data.trim().is_empty() {
        return Ok(Val::default())
    }

    let mut p = Parser::new();
    {
        let lex = Token::lexer(data);
        let tks = lex.collect::<Vec<_>>();
        event!(Level::TRACE, ?tks, "HIGHLIGHT LEX");
    }
    let mut lex = Token::lexer(data);
    while let Some(tk) = lex.next() {
        p.parse(tk)
            .map_err(|_| {
                Kind::PomeloError{span: lex.span(), text: lex.slice().into()}
            })
            .in_current_span()?
    }

    let items = p.end_of_input()
        .map_err(|_| {
            Kind::PomeloError{span: lex.span(), text: lex.slice().into()}
        })?;

    event!(Level::TRACE, ?items, "HIGHLIGHT PARSE");
    eprintln!("HIGHLIGHT PARSE {items:#?}");

    let mut val = eval(&items[..]);

    event!(Level::TRACE, ?val, "HIGHLIGHT EVAL");
    eprintln!("HIGHLIGHT EVAL {val:#?}");

    let mut scopes = HashMap::new();
    let val2 = val.clone();
    index(&val2, &mut vec![], &mut scopes);
    resolve(&mut val, &mut vec![], &scopes);

    eprintln!("HIGHLIGHT SCOPES: {scopes:#?}");
    eprintln!("HIGHLIGHT RESOLVE: {val:#?}");
    Ok(val)
}

pub fn app(cx: Scope<AppProps>) -> Element {
    let model = use_state(&cx, || String::from(PLACEHOLDER));
    let drawing = use_state(&cx, Drawing::default);
    let highlight = use_state(&cx, || String::from(""));

    let drawing_sender = use_coroutine(cx, |mut rx| { 
        let drawing = drawing.clone();
        async move {
            while let Some(msg) = rx.next().await {
                drawing.set(msg);
            }
        }
    });

    let model_sender = use_coroutine(cx, |mut rx| {
        let drawing_sender = drawing_sender.clone();
        async move {
            let mut prev_model: Option<String> = None;
            while let Some(model) = rx.next().await {
                if Some(&model) != prev_model.as_ref() {
                    let model_str: &str = &model;
                    let nodes = if model_str.trim().is_empty() {
                        Ok(Ok(Drawing::default()))
                    } else {
                        catch_unwind(|| {
                            draw(model.clone())
                        })
                    };
                    let model = model.clone();
                    match nodes {
                        Ok(Ok(drawing)) => {
                            prev_model = Some(model);
                            drawing_sender.send(drawing);
                        },
                        Ok(Err(err)) => {
                            if let Some(st) = err.span_trace() {
                                let st_col = colorize(st);
                                event!(Level::ERROR, ?err, %st_col, "DRAWING ERROR SPANTRACE");
                            } else {
                                event!(Level::ERROR, ?err, "DRAWING ERROR");
                            }
                        }
                        Err(_) => {
                            event!(Level::ERROR, ?nodes, "PANIC");
                        }
                    }
                }
            }
        }
    });

    // let desktop = cx.consume_context::<dioxus_desktop::desktop_context::DesktopContext>().unwrap();
    // desktop.devtool();
    // let window = use_window(&cx);
    // window.devtool();

    let nodes = render(cx, drawing.get().clone());
    let logs = render_logs(cx, drawing.get().clone());

    let mut show_logs = use_state(&cx, || true);

    model_sender.send(model.get().clone());

    let viewbox_width = drawing.get().viewbox_width;
    let viewbox_height = drawing.get().viewbox_height;
    let _crossing_number = cx.render(rsx!(match drawing.get().crossing_number {
        Some(cn) => rsx!(span { "{cn}" }),
        None => rsx!(div{}),
    }));

    let data_svg = as_data_svg(drawing.get().clone());
    
    // parse and eval the highlight string to get a sub-model to highlight
    let highlight_styles = match parse_highlights(&highlight.get()[..]) {
        Ok(Val::Process { name, label, body: Some(Body::All(bs)) }) => {
            // cx.render(rsx!{"OOPS"})
            cx.render(rsx!{
                bs.iter().map(|b| {
                    match b {
                        Val::Process { name: Some(pname), .. } | Val::Process { label: Some(pname), .. } => {
                            let style = format!(".box.highlight_{pname} {{ background-color: red; color: white; }} .highlight_{pname} {{ color: red; }}");
                            eprintln!("STYLE: {style}");
                            rsx!{
                                style {
                                    "{style}"
                                }
                            }
                        },
                        Val::Chain{ name: Some(cname), .. } => {
                            let style = format!(".arrow.highlight_{cname} {{ color: red; }}");
                            eprintln!("STYLE: {style}");
                            rsx!{
                                style {
                                    "{style}"
                                }
                            }
                        }
                        Val::Chain{ path, .. } => {
                            rsx!{
                                path.windows(2).map(|pq| {
                                    match &pq[0] {
                                        Val::Process { name: Some(pname), .. } | Val::Process { label: Some(pname), .. } => {
                                            match &pq[1] {
                                                Val::Process { name: Some(qname), .. } | Val::Process { label: Some(qname), .. } => {
                                                    let style = format!(".arrow.{pname}_{qname} svg > path {{ stroke: red; }}");
                                                    eprintln!("STYLE: {style}");
                                                    rsx!{
                                                        style {
                                                            "{style}"
                                                        }
                                                    }
                                                }
                                                _ => {
                                                    eprintln!("UNSTYLE CHAIN WINDOW: {pq:#?}");
                                                    rsx!{()}
                                                }
                                            }
                                        }
                                        _ => {
                                            eprintln!("UNSTYLE CHAIN: {pq:#?}");
                                            rsx!{()}
                                        }
                                    }
                                })
                            }
                        }
                        _ => {
                            eprintln!("UNSTYLE: {b:#?}");
                            rsx!{()}
                        }
                    }
                })
            })
        },
        Err(e) => {
            let e = format!("{e:#?}");
            cx.render(rsx!{
                div {
                    e
                }
            })
        },
        _ => {
            cx.render(rsx!{()})
        }
    };

    let syntax_guide = depict::graph_drawing::frontend::dioxus::syntax_guide(cx)?;

    let style_default = "
        svg { stroke: currentColor; stroke-width: 1; } 
        .fake svg { stroke: hsl(0, 0%, 50%); } 
        path { stroke-dasharray: none; } 
        .arrow.fake path { stroke-dasharray: 5; }
        .keyword { font-weight: bold; color: rgb(207, 34, 46); }
        .example { font-size: 0.625rem; font-family: ui-monospace,SFMono-Regular,Menlo,Monaco,Consolas,\"Liberation Mono\",\"Courier New\",monospace; }
    ";
    cx.render(rsx!{
        head {
            style {
                "{style_default}"
            }
            highlight_styles
        }
        div {
            // key: "editor",
            style: "width: 100%; z-index: 20; padding: 1rem;",
            div {
                style: "max-width: 36rem; margin-left: auto; margin-right: auto; flex-direction: column;",
                div {
                    // key: "editor_label",
                    "Model"
                }
                div {
                    // key: "editor_editor",
                    textarea {
                        style: "border-width: 1px; border-color: #000;",
                        rows: "6",
                        cols: "80",
                        autocomplete: "off",
                        // autocorrect: "off",
                        // autocapitalize: "off",
                        "autocapitalize": "off",
                        autofocus: "true",
                        spellcheck: "false",
                        // placeholder: "",
                        oninput: move |e| { 
                            event!(Level::TRACE, "INPUT");
                            model.set(e.value.clone());
                            model_sender.send(e.value.clone());
                        },
                        "{model}"
                    }
                }
                div {
                    "Sub-model to Highlight"
                }
                div {
                    textarea {
                        style: "border-width 1px; border-color: #000;",
                        rows: "1",
                        cols: "80",
                        autocomplete: "off",
                        "autocapitalize": "off",
                        spellcheck: "false",
                        oninput: move |e| {
                            event!(Level::TRACE, "HIGHLIGHT INPUT");
                            highlight.set(e.value.clone());
                        }
                    }
                }
                div {
                    style: "display: flex; flex-direction: row; justify-content: space-between;",
                    syntax_guide,
                    div {
                        details {
                            style: "display: flex; flex-direction: column; align-self: end; font-size: 0.875rem; line-height: 1.25rem;",
                            summary {
                                span {
                                    "Tools",
                                }
                            },
                            // EXPORT
                            div {
                                a {
                                    href: "{data_svg}",
                                    download: "depict.svg",
                                    "Export SVG"
                                }
                            }
                            // LICENSES
                            div {
                                details {
                                    summary {
                                        style: "font-size: 0.875rem; line-height: 1.25rem; --tw-text-opacity: 1; color: rgba(156, 163, 175, var(--tw-text-opacity));",
                                        "Licenses",
                                    },
                                    div {
                                        (depict::licenses::LICENSES.dirs().map(|dir| {
                                            let path = dir.path().display();
                                            cx.render(rsx!{
                                                div {
                                                    key: "{path}",
                                                    span {
                                                        style: "font-style: italic; text-decoration: underline;",
                                                        "{path}"
                                                    },
                                                    ul {
                                                        dir.files().map(|f| {
                                                            let file_path = f.path();
                                                            let file_contents = f.contents_utf8().unwrap();
                                                            cx.render(rsx!{
                                                                details {
                                                                    key: "{file_path:?}",
                                                                    style: "white-space: pre;",
                                                                    summary {
                                                                        "{file_path:?}"
                                                                    }
                                                                    "{file_contents}"
                                                                }
                                                            })
                                                        })
                                                    }
                                                }
                                            })
                                        }))
                                    }
                                }
                            }
                            // LOG CONTROLS
                            div {
                                button {
                                    onclick: move |_| show_logs.modify(|v| !v),
                                    "Show debug logs"
                                }
                            }
                        }
                    }
                }
                // div {
                //     style: "font-size: 0.875rem; line-height: 1.25rem; --tw-text-opacity: 1; color: rgba(156, 163, 175, var(--tw-text-opacity)); width: 100%;",
                //     span {
                //         style: "color: #000;",
                //         "Crossing Number: "
                //     }
                //     span {
                //         style: "font-style: italic;",
                //         crossing_number
                //     }
                // }
            }
        }
        // DRAWING
        div {
            div {
                style: "position: relative; width: {viewbox_width}px; height: {viewbox_height}px; margin-left: auto; margin-right: auto; border-width: 1px; border-color: #000; margin-bottom: 40px;",
                nodes
            }
        }
        // LOGS
        div {
            style: "display: flex; flex-direction: row; overflow-x: auto;",
            show_logs.then(|| rsx!{
                logs
            })
        }
    })
}

pub fn main() -> io::Result<()> {    
    tracing_subscriber::Registry::default()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let mut menu_bar = tao::menu::MenuBar::new();
    let mut app_menu = tao::menu::MenuBar::new();
    let mut edit_menu = tao::menu::MenuBar::new();

    edit_menu.add_native_item(tao::menu::MenuItem::Undo);
    edit_menu.add_native_item(tao::menu::MenuItem::Redo);
    edit_menu.add_native_item(tao::menu::MenuItem::Separator);
    edit_menu.add_native_item(tao::menu::MenuItem::Cut);
    edit_menu.add_native_item(tao::menu::MenuItem::Copy);
    edit_menu.add_native_item(tao::menu::MenuItem::Paste);
    edit_menu.add_native_item(tao::menu::MenuItem::Separator);
    edit_menu.add_native_item(tao::menu::MenuItem::SelectAll);

    app_menu.add_native_item(tao::menu::MenuItem::CloseWindow);
    app_menu.add_native_item(tao::menu::MenuItem::Quit);
    menu_bar.add_submenu("Depict", true, app_menu);
    menu_bar.add_submenu("Edit", true, edit_menu);

    dioxus_desktop::launch_with_props(app,
        AppProps {},
        Config::new().with_window(
            WindowBuilder::new()
                .with_inner_size(LogicalSize::new(1200.0f64, 700.0f64))
                .with_title("Depict")
                .with_menu(menu_bar)
        ));

    // let mut vdom = VirtualDom::new(app);
    // let _ = vdom.rebuild();
    // let text = render_vdom(&vdom);
    // eprintln!("{text}");

    Ok(())
}
