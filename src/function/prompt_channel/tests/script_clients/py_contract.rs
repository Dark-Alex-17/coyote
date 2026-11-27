use super::*;

const PY_DRIVER: &str = "import importlib.util, sys\n\
    sys.dont_write_bytecode = True\n\
    spec = importlib.util.spec_from_file_location(\"prompt_utils\", sys.argv[1])\n\
    prompt_utils = importlib.util.module_from_spec(spec)\n\
    spec.loader.exec_module(prompt_utils)\n";

fn py_command(expr: &str) -> Command {
    let mut cmd = Command::new("python3");
    cmd.arg("-c")
        .arg(format!("{PY_DRIVER}print(prompt_utils.{expr})\n"))
        .arg(PY_HELPER);
    cmd.env("PYTHONDONTWRITEBYTECODE", "1");
    cmd
}

fn run_py(expr: &str, answers: Option<&[Value]>, stray_first: bool) -> Run {
    drive(py_command(expr), answers, stray_first)
}

fn run_py_in(expr: &str, answers: Option<&[Value]>, env: &[(&str, &str)], cwd: &Path) -> Run {
    let mut cmd = py_command(expr);
    cmd.current_dir(cwd);
    drive_env(cmd, answers, false, env)
}

#[test]
fn helper_honours_the_channel_contract() {
    if !python_available() {
        return;
    }
    check_contract(
        run_py,
        &Contract {
            confirm_true: "True",
            confirm_false: "False",
            none: "None",
            checked: "[0, 2]",
            typed: "typed by hand",
        },
    );
}

#[test]
fn guards_mirror_the_bash_contract() {
    if !python_available() {
        return;
    }
    let cwd = scratch_dir("py-guards");
    check_guards(&run_py_in, "None", &cwd);
    fs::remove_dir_all(&cwd).ok();
}

#[test]
fn replies_of_the_wrong_shape_are_unusable() {
    if !python_available() {
        return;
    }
    check_unusable_shapes(run_py, "None");
}

#[test]
fn no_channel_and_no_terminal_falls_closed_without_a_request() {
    if !python_available() {
        return;
    }
    check_no_channel(
        drive_without_channel(py_command("confirm(\"Go?\")")),
        "None",
    );
}

#[test]
fn request_lines_carry_the_text_verbatim_with_fresh_ids() {
    if !python_available() {
        return;
    }
    check_wire(run_py, "prompt_utils.", "x", "1");
}

#[test]
fn requests_escape_a_bash_tools_capture_through_its_saved_stdout() {
    if !python_available() || bash_program().is_none() {
        return;
    }
    check_nested_in_bash(&py_command("select(\"Pick\", [\"a\", \"b\", \"c\"])"), "1");
}

#[test]
fn wire_edges_mirror_the_bash_helper() {
    if !python_available() {
        return;
    }
    check_edges(py_command, "None", "True", "[]", "");
}

#[test]
fn dead_channel_and_answerless_reply_fail_closed() {
    if !python_available() {
        return;
    }
    let dir = scratch_dir("py-probe-edges");
    check_probe_edges(py_command, "None", "[0]", &dir);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_directory_as_reply_path_fails_closed_without_a_traceback() {
    if !python_available() {
        return;
    }
    let dir = scratch_dir("py-directory-reply");
    check_directory_reply(py_command, "None", &dir);
    fs::remove_dir_all(&dir).ok();
}
