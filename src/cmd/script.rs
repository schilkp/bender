// Copyright (c) 2017-2018 ETH Zurich
// Fabian Schuiki <fschuiki@iis.ee.ethz.ch>

//! The `script` subcommand.

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use clap::builder::PossibleValue;
use clap::{value_parser, Arg, ArgAction, ArgMatches, Command};
use indexmap::IndexSet;
use tera::{Context, Tera};
use tokio::runtime::Runtime;

use crate::error::*;
use crate::sess::{Session, SessionIo};
use crate::src::{SourceFile, SourceGroup};
use crate::target::{TargetSet, TargetSpec};

/// Assemble the `script` subcommand.
pub fn new() -> Command {
    Command::new("script")
        .about("Emit tool scripts for the package")
        .arg(
            Arg::new("target")
                .short('t')
                .long("target")
                .help("Only include sources that match the given target")
                .num_args(1)
                .action(ArgAction::Append)
                .value_parser(value_parser!(String)),
        )
        .arg(
            Arg::new("no-default-target")
                .long("no-default-target")
                .help("Remove any default targets that may be added to the generated script")
                .num_args(0)
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("format")
                .help("Format of the generated script")
                .required(true)
                .num_args(1)
                .value_parser([
                    PossibleValue::new("flist"),
                    PossibleValue::new("vsim"),
                    PossibleValue::new("vcs"),
                    PossibleValue::new("verilator"),
                    PossibleValue::new("synopsys"),
                    PossibleValue::new("formality"),
                    PossibleValue::new("riviera"),
                    PossibleValue::new("genus"),
                    PossibleValue::new("vivado"),
                    PossibleValue::new("vivado-sim"),
                    PossibleValue::new("precision"),
                    PossibleValue::new("template"),
                    PossibleValue::new("template_json"),
                ]),
        )
        .arg(
            Arg::new("relative-path")
                .long("relative-path")
                .num_args(0)
                .action(ArgAction::SetTrue)
                .help("Use relative paths (flist generation only)"),
        )
        .arg(
            Arg::new("define")
                .short('D')
                .long("define")
                .help("Pass an additional define to all source files")
                .num_args(1..)
                .action(ArgAction::Append)
                .value_parser(value_parser!(String)),
        )
        .arg(
            Arg::new("vcom-arg")
                .long("vcom-arg")
                .help("Pass an argument to vcom calls (vsim/vhdlan/riviera only)")
                .num_args(1..)
                .action(ArgAction::Append)
                .value_parser(value_parser!(String)),
        )
        .arg(
            Arg::new("vlog-arg")
                .long("vlog-arg")
                .help("Pass an argument to vlog calls (vsim/vlogan/riviera only)")
                .num_args(1..)
                .action(ArgAction::Append)
                .value_parser(value_parser!(String)),
        )
        .arg(
            Arg::new("only-defines")
                .long("only-defines")
                .num_args(0)
                .action(ArgAction::SetTrue)
                .help("Only output commands to define macros (Vivado only)"),
        )
        .arg(
            Arg::new("only-includes")
                .long("only-includes")
                .num_args(0)
                .action(ArgAction::SetTrue)
                .help("Only output commands to define include directories (Vivado only)"),
        )
        .arg(
            Arg::new("only-sources")
                .long("only-sources")
                .num_args(0)
                .action(ArgAction::SetTrue)
                .help("Only output commands to define source files (Vivado only)"),
        )
        .arg(
            Arg::new("no-simset")
                .long("no-simset")
                .num_args(0)
                .action(ArgAction::SetTrue)
                .help("Do not change `simset` fileset (Vivado only)"),
        )
        .arg(
            Arg::new("vlogan-bin")
                .long("vlogan-bin")
                .help("Specify a `vlogan` command")
                .num_args(1)
                .default_value("vlogan")
                .value_parser(value_parser!(String)),
        )
        .arg(
            Arg::new("vhdlan-bin")
                .long("vhdlan-bin")
                .help("Specify a `vhdlan` command")
                .num_args(1)
                .default_value("vhdlan")
                .value_parser(value_parser!(String)),
        )
        .arg(
            Arg::new("no-abort-on-error")
                .long("no-abort-on-error")
                .num_args(0)
                .action(ArgAction::SetTrue)
                .help("Do not abort analysis/compilation on first caught error (only for programs that support early aborting)")
        )
        .arg(
            Arg::new("compilation_mode")
                .long("compilation-mode")
                .help("Choose compilation mode option: separate/common")
                .num_args(1)
                .default_value("separate")
                .value_parser([
                    PossibleValue::new("separate"),
                    PossibleValue::new("common"),
                ])
        )
        .arg(
            Arg::new("package")
                .short('p')
                .long("package")
                .help("Specify package to show sources for")
                .num_args(1)
                .action(ArgAction::Append)
                .value_parser(value_parser!(String)),
        )
        .arg(
            Arg::new("no_deps")
                .short('n')
                .long("no-deps")
                .num_args(0)
                .action(ArgAction::SetTrue)
                .help("Exclude all dependencies, i.e. only top level or specified package(s)"),
        )
        .arg(
            Arg::new("exclude")
                .short('e')
                .long("exclude")
                .help("Specify package to exclude from sources")
                .num_args(1)
                .action(ArgAction::Append)
                .value_parser(value_parser!(String)),
        )
        .arg(
            Arg::new("template")
                .long("template")
                .required_if_eq("format", "template")
                .help("Path to a file containing the tera template string to be formatted.")
                .num_args(1)
                .value_parser(value_parser!(String)),
        )
}

fn get_package_strings<I>(packages: I) -> IndexSet<String>
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    packages
        .into_iter()
        .map(|t| t.as_ref().to_string().to_lowercase())
        .collect()
}

/// Execute the `script` subcommand.
pub fn run(sess: &Session, matches: &ArgMatches) -> Result<()> {
    let rt = Runtime::new()?;
    let io = SessionIo::new(sess);
    let mut srcs = rt.block_on(io.sources())?;

    // Format-specific target specifiers.
    let vivado_targets = &["vivado", "fpga", "xilinx"];
    fn concat<T: Clone>(a: &[T], b: &[T]) -> Vec<T> {
        a.iter().chain(b).cloned().collect()
    }
    let format = matches.get_one::<String>("format").unwrap();
    let format_targets: Vec<&str> = if !matches.get_flag("no-default-target") {
        match format.as_str() {
            "flist" => vec!["flist"],
            "vsim" => vec!["vsim", "simulation"],
            "vcs" => vec!["vcs", "simulation"],
            "verilator" => vec!["verilator", "synthesis"],
            "synopsys" => vec!["synopsys", "synthesis"],
            "formality" => vec!["synopsys", "synthesis", "formality"],
            "riviera" => vec!["riviera", "simulation"],
            "genus" => vec!["genus", "synthesis"],
            "vivado" => concat(vivado_targets, &["synthesis"]),
            "vivado-sim" => concat(vivado_targets, &["simulation"]),
            "precision" => vec!["precision", "fpga", "synthesis"],
            "template" => vec![],
            "template_json" => vec![],
            _ => unreachable!(),
        }
    } else {
        vec![]
    };

    // Filter the sources by target.
    let targets = matches
        .get_many::<String>("target")
        .map(|t| {
            TargetSet::new(
                t.map(|element| element.as_str())
                    .chain(format_targets.clone()),
            )
        })
        .unwrap_or_else(|| TargetSet::new(format_targets));
    srcs = srcs
        .filter_targets(&targets)
        .unwrap_or_else(|| SourceGroup {
            package: Default::default(),
            independent: true,
            target: TargetSpec::Wildcard,
            include_dirs: Default::default(),
            export_incdirs: Default::default(),
            defines: Default::default(),
            files: Default::default(),
            dependencies: Default::default(),
            version: None,
        });

    // Filter the sources by specified packages.
    let packages = &srcs.get_package_list(
        sess,
        &matches
            .get_many::<String>("package")
            .map(get_package_strings)
            .unwrap_or_default(),
        &matches
            .get_many::<String>("exclude")
            .map(get_package_strings)
            .unwrap_or_default(),
        matches.get_flag("no_deps"),
    );

    if matches.contains_id("package")
        || matches.contains_id("exclude")
        || matches.get_flag("no_deps")
    {
        srcs = srcs
            .filter_packages(packages)
            .unwrap_or_else(|| SourceGroup {
                package: Default::default(),
                independent: true,
                target: TargetSpec::Wildcard,
                include_dirs: Default::default(),
                export_incdirs: Default::default(),
                defines: Default::default(),
                files: Default::default(),
                dependencies: Default::default(),
                version: None,
            });
    }

    // Flatten the sources.
    let srcs = srcs.flatten();

    // Validate format-specific options.
    if (matches.contains_id("vcom-arg") || matches.contains_id("vlog-arg"))
        && format != "vsim"
        && format != "vcs"
        && format != "riviera"
        && format != "template"
        && format != "template_json"
    {
        return Err(Error::new(
            "vsim/vcs-only options can only be used for 'vcs', 'vsim' or 'riviera' format!",
        ));
    }
    if (matches.get_flag("only-defines")
        || matches.get_flag("only-includes")
        || matches.get_flag("only-sources")
        || matches.get_flag("no-simset"))
        && !format.starts_with("vivado")
        && format != "template"
        && format != "template_json"
    {
        return Err(Error::new(
            "Vivado-only options can only be used for 'vivado' format!",
        ));
    }

    // Generate the corresponding output.
    match format.as_str() {
        "flist" => emit_template(sess, FLIST_TPL, matches, targets, srcs),
        "vsim" => emit_template(sess, VSIM_TCL_TPL, matches, targets, srcs),
        "vcs" => emit_template(sess, VCS_SH_TPL, matches, targets, srcs),
        "verilator" => emit_template(sess, VERILATOR_SH_TPL, matches, targets, srcs),
        "synopsys" => emit_template(sess, SYNOPSYS_TCL_TPL, matches, targets, srcs),
        "formality" => emit_template(sess, FORMALITY_TCL_TPL, matches, targets, srcs),
        "riviera" => emit_template(sess, RIVIERA_TCL_TPL, matches, targets, srcs),
        "genus" => emit_template(sess, GENUS_TCL_TPL, matches, targets, srcs),
        "vivado" => emit_template(sess, VIVADO_TCL_TPL, matches, targets, srcs),
        "vivado-sim" => emit_template(sess, VIVADO_TCL_TPL, matches, targets, srcs),
        "precision" => emit_template(sess, PRECISION_TCL_TPL, matches, targets, srcs),
        "template" => {
            let custom_tpl_path = Path::new(matches.get_one::<String>("template").unwrap());
            let custom_tpl_str =
                &String::from_utf8(fs::read(custom_tpl_path)?).map_err(|e| Error::chain("", e))?;
            emit_template(sess, custom_tpl_str, matches, targets, srcs)
        }
        "template_json" => emit_template(sess, JSON, matches, targets, srcs),
        _ => unreachable!(),
    }
}

/// Subdivide the source files in a group.
///
/// The function `cateogrize` is used to assign a category to each source file.
/// Files with the same category that appear after each other will be kept in
/// the same source group. Files with different cateogries are split into
/// separate groups.
fn separate_files_in_group<F1, F2, T>(mut src: SourceGroup, categorize: F1, mut consume: F2)
where
    F1: Fn(&SourceFile) -> Option<T>,
    F2: FnMut(&SourceGroup, T, Vec<SourceFile>),
    T: Eq,
{
    let mut category = None;
    let mut files = vec![];
    for file in std::mem::take(&mut src.files) {
        let new_category = categorize(&file);
        if new_category.is_none() {
            continue;
        }
        if category.is_some() && category != new_category && !files.is_empty() {
            consume(&src, category.take().unwrap(), std::mem::take(&mut files));
        }
        files.push(file);
        category = new_category;
    }
    if !files.is_empty() {
        consume(&src, category.unwrap(), files);
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum SourceType {
    Verilog,
    Vhdl,
}

fn relativize_path(path: &std::path::Path, root: &std::path::Path) -> String {
    if path.starts_with(root) {
        format!(
            "$ROOT/{}",
            path.strip_prefix(root).unwrap().to_str().unwrap()
        )
    } else {
        path.to_str().unwrap().to_string()
    }
}

static HEADER_AUTOGEN: &str = "This script was generated automatically by bender.";

fn add_defines_from_matches(defines: &mut Vec<(String, Option<String>)>, matches: &ArgMatches) {
    if let Some(d) = matches.get_many::<String>("define") {
        defines.extend(d.map(|t| {
            let mut parts = t.splitn(2, '=');
            let name = parts.next().unwrap().trim(); // split always has at least one element
            let value = parts.next().map(|v| v.trim().to_string());
            (name.to_string(), value)
        }));
    }
}

static JSON: &str = "json";

fn emit_template(
    sess: &Session,
    template: &str,
    matches: &ArgMatches,
    targets: TargetSet,
    srcs: Vec<SourceGroup>,
) -> Result<()> {
    let mut tera_obj = Tera::default();
    let mut tera_context = Context::new();
    tera_context.insert("HEADER_AUTOGEN", HEADER_AUTOGEN);
    tera_context.insert("root", sess.root);
    // tera_context.insert("srcs", &srcs);
    tera_context.insert("abort_on_error", &!matches.get_flag("no-abort-on-error"));

    let mut defines: Vec<(String, Option<String>)> = vec![];
    defines.extend(
        targets
            .iter()
            .map(|t| (format!("TARGET_{}", t.to_uppercase()), None)),
    );
    add_defines_from_matches(&mut defines, matches);
    defines.sort();
    tera_context.insert("global_defines", &defines);

    let mut all_defines = defines.clone();
    let mut all_incdirs = vec![];
    let mut all_files = vec![];
    let mut all_verilog = vec![];
    let mut all_vhdl = vec![];
    for src in &srcs {
        all_defines.extend(
            src.defines
                .iter()
                .map(|(k, &v)| (k.to_string(), v.map(String::from))),
        );
        all_incdirs.append(&mut src.clone().get_incdirs());
        all_files.append(&mut src.files.clone());
    }
    let all_defines = if !matches.get_flag("only-includes") && !matches.get_flag("only-sources") {
        all_defines
    } else {
        vec![]
    };
    tera_context.insert("all_defines", &all_defines);

    let all_incdirs: IndexSet<PathBuf> =
        if !matches.get_flag("only-defines") && !matches.get_flag("only-sources") {
            all_incdirs.into_iter().map(|p| p.to_path_buf()).collect()
        } else {
            IndexSet::new()
        };
    tera_context.insert("all_incdirs", &all_incdirs);
    let all_files: IndexSet<PathBuf> =
        if !matches.get_flag("only-defines") && !matches.get_flag("only-includes") {
            all_files
                .into_iter()
                .filter_map(|file| match file {
                    SourceFile::File(p) => Some(p.to_path_buf()),
                    _ => None,
                })
                .collect()
        } else {
            IndexSet::new()
        };
    tera_context.insert("all_files", &all_files);

    let mut split_srcs = vec![];
    for src in srcs {
        separate_files_in_group(
            src,
            |f| match f {
                SourceFile::File(p) => match p.extension().and_then(std::ffi::OsStr::to_str) {
                    Some("sv") | Some("v") | Some("vp") => Some(SourceType::Verilog),
                    Some("vhd") | Some("vhdl") => Some(SourceType::Vhdl),
                    _ => None,
                },
                _ => None,
            },
            |src, ty, files| {
                split_srcs.push(TplSrcStruct {
                    defines: {
                        let mut local_defines = defines.clone();
                        local_defines.extend(
                            src.defines
                                .iter()
                                .map(|(k, &v)| (k.to_string(), v.map(String::from))),
                        );
                        local_defines
                    },
                    incdirs: src
                        .clone()
                        .get_incdirs()
                        .iter()
                        .map(|p| p.to_path_buf())
                        .collect(),
                    files: files
                        .iter()
                        .map(|f| match f {
                            SourceFile::File(p) => p.to_path_buf(),
                            SourceFile::Group(_) => unreachable!(),
                        })
                        .collect(),
                    file_type: match ty {
                        SourceType::Verilog => "verilog".to_string(),
                        SourceType::Vhdl => "vhdl".to_string(),
                    },
                });
            },
        );
    }
    for src in &split_srcs {
        match src.file_type.as_str() {
            "verilog" => {
                all_verilog.append(&mut src.files.clone());
            }
            "vhdl" => {
                all_vhdl.append(&mut src.files.clone());
            }
            _ => {}
        }
    }
    let split_srcs = if !matches.get_flag("only-defines") && !matches.get_flag("only-includes") {
        split_srcs
    } else {
        vec![]
    };
    tera_context.insert("srcs", &split_srcs);

    let all_verilog: IndexSet<PathBuf> =
        if !matches.get_flag("only-defines") && !matches.get_flag("only-includes") {
            all_verilog.into_iter().collect()
        } else {
            IndexSet::new()
        };
    let all_vhdl: IndexSet<PathBuf> =
        if !matches.get_flag("only-defines") && !matches.get_flag("only-includes") {
            all_vhdl.into_iter().collect()
        } else {
            IndexSet::new()
        };
    tera_context.insert("all_verilog", &all_verilog);
    tera_context.insert("all_vhdl", &all_vhdl);

    let vlog_args: Vec<String> = if let Some(args) = matches.get_many::<String>("vlog-arg") {
        args.map(Into::into).collect()
    } else {
        [].to_vec()
    };
    tera_context.insert("vlog_args", &vlog_args);
    let vcom_args: Vec<String> = if let Some(args) = matches.get_many::<String>("vcom-arg") {
        args.map(Into::into).collect()
    } else {
        [].to_vec()
    };
    tera_context.insert("vcom_args", &vcom_args);

    tera_context.insert("vlogan_bin", &matches.get_one::<String>("vlogan-bin"));
    tera_context.insert("vhdlan_bin", &matches.get_one::<String>("vhdlan-bin"));
    tera_context.insert("relativize_path", &matches.get_flag("relative-path"));
    tera_context.insert(
        "compilation_mode",
        &matches.get_one::<String>("compilation_mode"),
    );

    let vivado_filesets = if matches.get_flag("no-simset") {
        vec![""]
    } else {
        vec!["", " -simset"]
    };

    tera_context.insert("vivado_filesets", &vivado_filesets);

    if template == "json" {
        println!("{:#}", tera_context.into_json());
        return Ok(());
    }

    print!(
        "{}",
        tera_obj
            .render_str(template, &tera_context)
            .map_err(|e| { Error::chain("Failed to render template.", e) })?
    );

    Ok(())
}

#[derive(Debug, Serialize)]
struct TplSrcStruct {
    defines: Vec<(String, Option<String>)>,
    incdirs: Vec<PathBuf>,
    files: Vec<PathBuf>,
    file_type: String,
}

static FLIST_TPL: &str = "\
{% for incdir in all_incdirs %}\
    {% if relativize_path %}\
        +incdir+{{ incdir | replace(from=root, to='') }}\n\
    {% else %}\
        +incdir+{{ incdir }}\n\
    {% endif %}\
{% endfor %}\
{% for define in all_defines %}\
    +define+{{ define.0 | upper }}{% if define.1 %}={{ define.1 }}{% endif %}\n\
{% endfor %}\
{% for file in all_files %}\
    {% if relativize_path %}\
        {% if file is starting_with(root) %}\
            {{ file | replace(from=root, to='') | trim_start_matches(pat='/') }}\n\
        {% else %}\
            {{ file }}\n\
        {% endif %}\
    {% else %}\
        {{ file }}\n\
    {% endif %}\
{% endfor %}";

static VSIM_TCL_TPL: &str = "\
# {{ HEADER_AUTOGEN }}
set ROOT \"{{ root }}\"
{% if compilation_mode == 'separate' %}\
    {% for group in srcs %}\n\
        {% if abort_on_error %}if {[catch { {% endif %}\
        {% if group.file_type == 'verilog' %}vlog -incr -sv \\\n    \
            {% for tmp_arg in vlog_args %}\
                {{ tmp_arg }} \\\n    \
            {% endfor %}\
            {% for define in group.defines %}\
                +define+{{ define.0 | upper }}{% if define.1 %}={{ define.1 }}{% endif %} \\\n    \
            {% endfor %}\
            {% for incdir in group.incdirs %}\
                \"+incdir+{{ incdir | replace(from=root, to='$ROOT') }}\" \\\n    \
            {% endfor %}\
        {% elif group.file_type == 'vhdl' %}vcom -2008 \\\n    \
            {% for tmp_arg in vcom_args %}\
                {{ tmp_arg }} \\\n    \
            {% endfor %}\
        {% endif %}\
        {% for file in group.files %}\
            \"{{ file | replace(from=root, to='$ROOT') }}\" {% if not loop.last %}\\\n    {% endif %}\
        {% endfor %}\
        {% if abort_on_error %}\\\n}]} {return 1}\
        {% endif %}\n\
    {% endfor %}\
{% else %}{# compilation_mode == 'common' #}\
    {% for file in all_verilog %}\
        {% if loop.first %}\
            {% if abort_on_error %}if {[catch { {% endif %}\
            vlog -incr -sv \\\n    \
            {% for tmp_arg in vlog_args %}\
                {{ tmp_arg }} \\\n    \
            {% endfor %}\
            {% for define in all_defines %}\
                +define+{{ define.0 | upper }}{% if define.1 %}={{ define.1 }}{% endif %} \\\n    \
            {% endfor %}\
            {% for incdir in all_incdirs %}\
                \"+incdir+{{ incdir | replace(from=root, to='$ROOT') }}\" \\\n    \
            {% endfor %}\
        {% endif %}\
        \"{{ file | replace(from=root, to='$ROOT') }}\" {% if not loop.last %}\\\n    {% endif %}\
        {% if loop.last %}\
            {% if abort_on_error %} \\\n}]} {return 1}\
            {% endif %}\n\
        {% endif %}\
    {% endfor %}\
    {% for file in all_vhdl %}\
        {% if loop.first %}\
            {% if abort_on_error %}if {[catch { {% endif %}\
            vcom -2008 \\\n    \
            {% for tmp_arg in vcom_args %}\
                {{ tmp_arg }} \\\n    \
            {% endfor %}\
        {% endif %}\
        \"{{ file | replace(from=root, to='$ROOT') }}\" {% if not loop.last %}\\\n    {% endif %}\
        {% if loop.last %}\n\
            {% if abort_on_error %}\n}]} {return 1}\
            {% endif %}\n\
        {% endif %}\
    {% endfor %}\
{% endif %}";

static VCS_SH_TPL: &str = "\
#!/usr/bin/env bash
# {{ HEADER_AUTOGEN }}
ROOT=\"{{ root }}\"
{% if compilation_mode == 'separate' %}\
    {% for group in srcs %}\n\
        {% if group.file_type == 'verilog' %}{{ vlogan_bin }} -sverilog \\\n    \
            -full64 \\\n    \
            {% for tmp_arg in vlog_args %}\
                {{ tmp_arg }} \\\n    \
            {% endfor %}\
            {% for define in group.defines %}\
                +define+{{ define.0 | upper }}{% if define.1 %}={{ define.1 }}{% endif %} \\\n    \
            {% endfor %}\
            {% for incdir in group.incdirs %}\
                \"+incdir+{{ incdir | replace(from=root, to='$ROOT') }}\" \\\n    \
            {% endfor %}\
        {% elif group.file_type == 'vhdl' %}{{ vhdlan_bin }} \\\n    \
            {% for tmp_arg in vcom_args %}\
                {{ tmp_arg }} \\\n    \
            {% endfor %}\
        {% endif %}\
        {% for file in group.files %}\
            \"{{ file | replace(from=root, to='$ROOT') }}\" {% if not loop.last %}\\\n    {% endif %}\
        {% endfor %}\n\
    {% endfor %}
{% else %}{# compilation_mode == 'common' #}\
    {% for file in all_verilog %}\
        {% if loop.first %}{{ vlogan_bin }} -sverilog \\\n    \
            -full64 \\\n    \
            {% for tmp_arg in vlog_args %}\
                {{ tmp_arg }} \\\n    \
            {% endfor %}\
            {% for define in all_defines %}\
                +define+{{ define.0 | upper }}{% if define.1 %}={{ define.1 }}{% endif %} \\\n    \
            {% endfor %}\
            {% for incdir in all_incdirs %}\
                \"+incdir+{{ incdir | replace(from=root, to='$ROOT') }}\" \\\n    \
            {% endfor %}\
        {% endif %}\
        \"{{ file | replace(from=root, to='$ROOT') }}\" {% if not loop.last %}\\\n    {% endif %}\
        {% if loop.last %}\n{% endif %}\
    {% endfor %}\n\
    {% for file in all_vhdl %}\
        {% if loop.first %}{{ vhdlan_bin }} \\\n    \
            {% for tmp_arg in vcom_args %}\
                {{ tmp_arg }} \\\n    \
            {% endfor %}\
        {% endif %}\
        \"{{ file | replace(from=root, to='$ROOT') }}\" {% if not loop.last %}\\\n    {% endif %}\
        {% if loop.last %}\n{% endif %}\
    {% endfor %}\n\
{% endif %}";

static VERILATOR_SH_TPL: &str = "\
{% for group in srcs %}\
    {% if group.file_type == 'verilog' %}\n\
        {% for tmp_arg in vlog_args %}\
            {{ tmp_arg }}\n\
        {% endfor %}\
        {% for define in group.defines %}\
            +define+{{ define.0 | upper }}{% if define.1 %}={{ define.1 }}{% endif %}\n\
        {% endfor %}\
        {% for incdir in group.incdirs %}\
            +incdir+{{ incdir | replace(from=root, to='$ROOT') }}\n\
        {% endfor %}\
        {% for file in group.files %}\
            {{ file }}\n\
        {% endfor %}\
    {% endif %}\
{% endfor %}";

static SYNOPSYS_TCL_TPL: &str = "\
# {{HEADER_AUTOGEN}}
set ROOT \"{{ root }}\"
set search_path_initial $search_path
{% if compilation_mode == 'separate' %}\
    {% for group in srcs %}\n\
        set search_path $search_path_initial\n\
        {% for incdir in group.incdirs %}\
            lappend search_path \"$ROOT{{ incdir | replace(from=root, to='') }}\"\n\
        {% endfor %}\n\
        {% if abort_on_error %}if {[catch { {% endif %}analyze -format \
        {% if group.file_type == 'verilog' %}sv{% elif group.file_type == 'vhdl' %}vhdl{% endif %} \\\n    \
        {% for define in group.defines %}\
            {% if loop.first %}-define { \\\n        {% endif %}\
            {{ define.0 | upper }}{% if define.1 %}={{ define.1 }}{% endif %}\
            {% if loop.last %} \\\n    } \\\n    {% else %} \\\n        {% endif %}\
        {% endfor %}\
        [list \\\n    \
        {% for file in group.files %}\
            {{ '    ' }}\"{{ file | replace(from=root, to='$ROOT') }}\" \\\n    \
        {% endfor %}\
        ]\n\
        {% if abort_on_error %}}]} {return 1}\
        {% endif %}\n\
    {% endfor %}\n\
{% else %}{# compilation_mode == 'common' #}\
    {% for file in all_verilog %}\
        {% if loop.first %}\
            set search_path $search_path_initial\n\
            {% for incdir in all_incdirs %}\
                lappend search_path \"$ROOT{{ incdir | replace(from=root, to='') }}\"\n\
            {% endfor %}\n\
            {% if abort_on_error %}if {[catch { {% endif %}analyze -format sv \\\n    \
            {% for define in all_defines %}\
                {% if loop.first %}-define { \\\n        {% endif %}\
                {{ define.0 | upper }}{% if define.1 %}={{ define.1 }}{% endif %}\
                {% if loop.last %} \\\n    } \\\n    {% else %} \\\n        {% endif %}\
            {% endfor %}\
            [list \\\n    \
        {% endif %}\
        {{ '    ' }}\"{{ file | replace(from=root, to='$ROOT') }}\" \\\n    \
        {% if loop.last %}\
            ]\n\
            {% if abort_on_error %}}]} {return 1}\
            {% endif %}\n\
        {% endif %}\
    {% endfor %}\n\
    {% for file in all_vhdl %}\
        {% if loop.first %}
            {% if abort_on_error %}if {[catch { {% endif %}analyze -format vhdl \\\n    \
            [list \\\n    \
        {% endif %}\
        {{ '    ' }}\"{{ file | replace(from=root, to='$ROOT') }}\" \\\n    \
        {% if loop.last %}\
            ]\n\
            {% if abort_on_error %}}]} {return 1}\
            {% endif %}\n\
        {% endif %}\
    {% endfor %}\n\
{% endif %}\
set search_path $search_path_initial\n";

static FORMALITY_TCL_TPL: &str = "\
# {{HEADER_AUTOGEN}}
set ROOT \"{{ root }}\"
set search_path_initial $search_path
{% if compilation_mode == 'separate' %}\
    {% for group in srcs %}\n\
        set search_path $search_path_initial\n\
        {% for incdir in group.incdirs %}\
            lappend search_path \"$ROOT{{ incdir | replace(from=root, to='') }}\"\n\
        {% endfor %}\n\
        {% if abort_on_error %}if {[catch { {% endif %}\
        {% if group.file_type == 'verilog' %}read_sverilog{% elif group.file_type == 'vhdl' %}read_vhdl{% endif %} -r \\\n    \
        {% for define in group.defines %}\
            {% if loop.first %}-define { \\\n        {% endif %}\
            {{ define.0 | upper }}{% if define.1 %}={{ define.1 }}{% endif %}\
            {% if loop.last %} \\\n    } \\\n    {% else %} \\\n        {% endif %}\
        {% endfor %}\
        [list \\\n    \
        {% for file in group.files %}\
            {{ '    ' }}\"{{ file | replace(from=root, to='$ROOT') }}\" \\\n    \
        {% endfor %}\
        ]\n\
        {% if abort_on_error %}}]} {return 1}\
        {% endif %}\n\
    {% endfor %}\n\
{% else %}{# compilation_mode == 'common' #}\
    {% for file in all_verilog %}\
        {% if loop.first %}\
            set search_path $search_path_initial\n\
            {% for incdir in all_incdirs %}\
                lappend search_path \"$ROOT{{ incdir | replace(from=root, to='') }}\"\n\
            {% endfor %}\n\
            {% if abort_on_error %}if {[catch { {% endif %}read_sverilog -r \\\n    \
            {% for define in all_defines %}\
                {% if loop.first %}-define { \\\n        {% endif %}\
                {{ define.0 | upper }}{% if define.1 %}={{ define.1 }}{% endif %}\
                {% if loop.last %} \\\n    } \\\n    {% else %} \\\n        {% endif %}\
            {% endfor %}\
            [list \\\n    \
        {% endif %}\
        {{ '    ' }}\"{{ file | replace(from=root, to='$ROOT') }}\" \\\n    \
        {% if loop.last %}\
            ]\n\
            {% if abort_on_error %}}]} {return 1}\
            {% endif %}\n\
        {% endif %}\
    {% endfor %}\n\
    {% for file in all_vhdl %}\
        {% if loop.first %}
            {% if abort_on_error %}if {[catch { {% endif %}read_vhdl -r \\\n    \
            [list \\\n    \
        {% endif %}\
        {{ '    ' }}\"{{ file | replace(from=root, to='$ROOT') }}\" \\\n    \
        {% if loop.last %}\
            ]\n\
            {% if abort_on_error %}}]} {return 1}\
            {% endif %}\n\
        {% endif %}\
    {% endfor %}\n\
{% endif %}\
set search_path $search_path_initial\n";

static GENUS_TCL_TPL: &str = "\
# {{ HEADER_AUTOGEN }}
if [ info exists search_path ] {{ '{{' }}
  set search_path_initial $search_path
{{ '}}' }}
set ROOT = \"{{ root }}\"
{% if compilation_mode == 'separate' %}\
    {% for group in srcs %}\n\
        set search_path $search_path_initial\n\
        {% for incdir in group.incdirs %}\
            lappend search_path \"$ROOT{{ incdir | replace(from=root, to='') }}\"\n\
        {% endfor %}\
        set_db init_hdl_search_path $search_path\n\n\
        {% if group.file_type == 'verilog' %}read_hdl -language sv \\\n    \
        {% elif group.file_type == 'vhdl' %}read_hdl -language vhdl \\\n    \
        {% endif %}\
        {% for define in group.defines %}\
            {% if loop.first %}-define { \\\n        {% endif %}\
            {{ define.0 | upper }}{% if define.1 %}={{ define.1 }}{% endif %}\
            {% if loop.last %} \\\n    } \\\n    {% else %} \\\n        {% endif %}\
        {% endfor %}\
        [list \\\n    \
        {% for file in group.files %}\
            {{ '    ' }}\"{{ file | replace(from=root, to='$ROOT') }}\" \\\n    \
        {% endfor %}\
        ]\n\
    {% endfor %}
{% else %}{# compilation_mode == 'common' #}\
    {% for file in all_verilog %}\
        {% if loop.first %}\
            set search_path $search_path_initial\n\
            {% for incdir in all_incdirs %}\
                lappend search_path \"$ROOT{{ incdir | replace(from=root, to='') }}\"\n\
            {% endfor %}\n\
            set_db init_hdl_search_path $search_path\n\n\
            {% if abort_on_error %}if {[catch { {% endif %}read_hdl -language sv \\\n    \
            {% for define in all_defines %}\
                {% if loop.first %}-define { \\\n        {% endif %}\
                {{ define.0 | upper }}{% if define.1 %}={{ define.1 }}{% endif %}\
                {% if loop.last %} \\\n    } \\\n    {% else %} \\\n        {% endif %}\
            {% endfor %}\
            [list \\\n    \
        {% endif %}\
        {{ '    ' }}\"{{ file | replace(from=root, to='$ROOT') }}\" \\\n    \
        {% if loop.last %}\
            ]\n\
            {% if abort_on_error %}}]} {return 1}\
            {% endif %}\n\
        {% endif %}\
    {% endfor %}\n\
    {% for file in all_vhdl %}\
        {% if loop.first %}
            {% if abort_on_error %}if {[catch { {% endif %}read_hdl -language vhdl \\\n    \
            [list \\\n    \
        {% endif %}\
        {{ '    ' }}\"{{ file | replace(from=root, to='$ROOT') }}\" \\\n    \
        {% if loop.last %}\
            ]\n\
            {% if abort_on_error %}}]} {return 1}\
            {% endif %}\n\
        {% endif %}\
    {% endfor %}\n\
{% endif %}\
set search_path $search_path_initial\n";

static VIVADO_TCL_TPL: &str = "\
# {{ HEADER_AUTOGEN }}
set ROOT \"{{ root }}\"
{% if compilation_mode == 'separate' %}\
    {% for group in srcs %}\
        add_files -norecurse -fileset [current_fileset] [list \\\n    \
        {% for file in group.files %}\
            {{ file | replace(from=root, to='$ROOT') }} \\\n{% if not loop.last %}    {% endif %}\
        {% endfor %}\
        ]\n\
    {% endfor %}\
{% else %}{# compilation_mode == 'common' #}\
    {% for file in all_files %}\
        {% if loop.first %}\
            add_files -norecurse -fileset [current_fileset] [list \\\n    \
        {% endif %}\
        {{ file | replace(from=root, to='$ROOT') }} \\\n{% if not loop.last %}    {% endif %}\
        {% if loop.last %}\
            ]\n\
        {% endif %}\
    {% endfor %}\
{% endif %}\
{% for arg in vivado_filesets %}\
    {% for incdir in all_incdirs %}\
        {% if loop.first %}\nset_property include_dirs [list \\\n    {% endif %}\
        {{incdir | replace(from=root, to='$ROOT') }}\
        {%if loop.last %} \\\n] [current_fileset{{ arg }}]\n{% else %} \\\n    {% endif %}\
    {% endfor %}\
{% endfor %}\
{% for arg in vivado_filesets %}\
    {% for define in all_defines %}\
        {% if loop.first %}\nset_property verilog_define [list \\\n    {% endif %}\
        {{ define.0 | upper }}{% if define.1 %}={{ define.1 }}{% endif %}\
        {% if loop.last %} \\\n] [current_fileset{{ arg }}]\n{% else %} \\\n    {% endif %}\
    {% endfor %}\
{% endfor %}";

static RIVIERA_TCL_TPL: &str = "\
# {{ HEADER_AUTOGEN }}
set ROOT \"{{ root }}\"
vlib work
{% if compilation_mode == 'separate' %}\
    {% for group in srcs %}\
        {% if abort_on_error %}if {[catch { {% endif %}\
        {% if group.file_type == 'verilog' %}vlog -sv \\\n    \
            {% for tmp_arg in vlog_args %}\
                {{ tmp_arg }} \\\n    \
            {% endfor %}\
            {% for define in group.defines %}\
                +define+{{ define.0 | upper }}{% if define.1 %}={{ define.1 }}{% endif %} \\\n    \
            {% endfor %}\
            {% for incdir in group.incdirs %}\
                \"+incdir+{{ incdir | replace(from=root, to='$ROOT') }}\" \\\n    \
            {% endfor %}\
        {% elif group.file_type == 'vhdl' %}vcom -2008 \\\n    \
            {% for tmp_arg in vcom_args %}\
                {{ tmp_arg }} \\\n    \
            {% endfor %}\
        {% endif %}\
        {% for file in group.files %}\
            \"{{ file | replace(from=root, to='$ROOT') }}\" {% if not loop.last %}\\\n    {% else %}\\\n{% endif %}\
        {% endfor %}\
        {% if abort_on_error %}}]} {return 1}\
        {% endif %}\n\n\
    {% endfor %}
{% else %}{# compilation_mode == 'common' #}\
    {% for file in all_verilog %}\
        {% if loop.first %}\
            {% if abort_on_error %}if {[catch { {% endif %}\
            vlog -sv \\\n    \
            {% for tmp_arg in vlog_args %}\
                {{ tmp_arg }} \\\n    \
            {% endfor %}\
            {% for define in all_defines %}\
                +define+{{ define.0 | upper }}{% if define.1 %}={{ define.1 }}{% endif %} \\\n    \
            {% endfor %}\
            {% for incdir in all_incdirs %}\
                \"+incdir+{{ incdir | replace(from=root, to='$ROOT') }}\" \\\n    \
            {% endfor %}\
        {% endif %}\
        \"{{ file | replace(from=root, to='$ROOT') }}\" {% if not loop.last %}\\\n    {% else %}\\\n{% endif %}\
        {% if loop.last %}\
            {% if abort_on_error %}}]} {return 1}\
            {% endif %}\n\n\
        {% endif %}\
    {% endfor %}\
    {% for file in all_vhdl %}\
        {% if loop.first %}\
            {% if abort_on_error %}if {[catch { {% endif %}\
            vcom -2008 \\\n    \
            {% for tmp_arg in vcom_args %}\
                {{ tmp_arg }} \\\n    \
            {% endfor %}\
        {% endif %}\
        \"{{ file | replace(from=root, to='$ROOT') }}\" {% if not loop.last %}\\\n    {% else %}\\\n{% endif %}\
        {% if loop.last %}\
            {% if abort_on_error %}}]} {return 1}\
            {% endif %}\n\n\
        {% endif %}\
    {% endfor %}\
{% endif %}";

static PRECISION_TCL_TPL: &str = "\
# {{ HEADER_AUTOGEN }}
# Precision does not take relative paths into account when specifying include dirs.
# Define the common ROOT anyway if needed for patching file paths.
set ROOT {{ root }}
set_input_dir $ROOT
setup_design -search_path $ROOT
{% for define in all_defines %}\
    {% if loop.first %}\
    \n# Set globally all defines for the (S)Verilog sources.\n\
    setup_design -defines { \\\n    {% endif %}\
    +define+{{ define.0 | upper }}{% if define.1 %}={{ define.1 }}{% endif %}\
    {% if loop.last %}\n\n{% else %} \\\n    {% endif %}\
{% endfor %}\
{% if compilation_mode == 'separate' %}\
    {% for group in srcs %}\
        {% if abort_on_error %}if {[catch { {% endif %}\
        add_input_file \\\n    \
        {% if group.file_type == 'verilog' %}\
            -format SystemVerilog2012 \\\n    \
            {% for incdir in group.incdirs %}\
                {% if loop.first %}-search_path { \\\n        {% endif %}\
                    {{ incdir }}\
                {% if loop.last %} \\\n    } \\\n    {% else %} \\\n        {% endif %}\
            {% endfor %}\
        {% elif group.file_type == 'vhdl' %}\
            -format vhdl_2008 \\\n    \
        {% endif %}\
        { \\\n        \
        {% for file in group.files %}\
            {{ file }}\
            {% if loop.last %} \\\n    {% else %} \\\n        {% endif %}\
        {% endfor %}\
        } \\\n\
        {% if abort_on_error %}}]} {return 1}\n\
        {% endif %}\n\
    {% endfor %}\n
{% else %}{# compilation_mode == 'common' #}\
    {% for file in all_verilog %}\
        {% if loop.first %}\
            {% if abort_on_error %}if {[catch { {% endif %}\
            add_input_file \\\n    \
            -format SystemVerilog2012 \\\n    \
            {% for incdir in all_incdirs %}\
                {% if loop.first %}-search_path { \\\n        {% endif %}\
                    {{ incdir }}\
                {% if loop.last %} \\\n    } \\\n    {% else %} \\\n        {% endif %}\
            {% endfor %}\
            { \\\n        \
        {% endif %}\
        {{ file }}\
        {% if loop.last %} \\\n    {% else %} \\\n        {% endif %}\
        {% if loop.last %}\
            } \\\n\
            {% if abort_on_error %}}]} {return 1}\n\
            {% endif %}\n\
        {% endif %}\
    {% endfor %}\n\
    {% for file in all_vhdl %}\
        {% if loop.first %}\
            {% if abort_on_error %}if {[catch { {% endif %}\
            add_input_file \\\n    \
            -format vhdl_2008 \\\n    \
        { \\\n        \
        {% endif %}\
        {{ file }}\
        {% if loop.last %} \\\n    {% else %} \\\n        {% endif %}\
        {% if loop.last %}\
            } \\\n\
            {% if abort_on_error %}}]} {return 1}\n\
            {% endif %}\n\
        {% endif %}\
    {% endfor %}\n\
{% endif %}";
