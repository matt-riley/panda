"""Exercise layered resources through terminal commands and captured provider requests."""
import json


def check(root, state, run, requests, saved_session):
    project = root / "resources"
    child = project / "nested"
    child.mkdir(parents=True)
    (project / ".git").mkdir()
    user_settings = state / "settings.json"
    project_settings = project / ".panda/settings.json"
    project_settings.parent.mkdir()
    project_settings.write_text('{"compaction":{"keepRecentTokens":20},"skillIndex":true}')
    user_settings.write_text('{"compaction":{"keepRecentTokens":10}}')

    def cli(args, stdin):
        return run(args, stdin, cwd=child)

    def display(flags=()):
        return "\n".join(cli(list(flags), ":settings reload\n:q\n"))

    assert "30 (--keep-recent-tokens)" in display(["--keep-recent-tokens", "30"])
    assert "20 (" in display() and str(project_settings.resolve()) in display()
    assert json.loads(project_settings.read_text())["compaction"]["keepRecentTokens"] == 20
    project_settings.unlink()
    assert "10 (" in display() and str(user_settings.resolve()) in display()
    user_settings.unlink()
    assert "automatic (default)" in display()
    project_settings.write_text('{"compaction":{"keepRecentTokens":"broken"}}')
    run(["--print", "probe"], "", cwd=child, error=str(project_settings.resolve()))
    project_settings.write_text('{"skillIndex":true}')

    local = project / ".pi"
    local.mkdir()
    for path, marker in [(state / "SYSTEM.md", "UserBase"), (local / "SYSTEM.md", "TrustedBase"),
                         (local / "APPEND_SYSTEM.md", "TrustedAppend"), (state / "APPEND_SYSTEM.md", "UserAppend"),
                         (project / "CLAUDE.md", "ParentFallback"), (child / "AGENTS.override.md", "ChildWinner"),
                         (child / "CLAUDE.md", "ChildFallback"), (child / "AGENTS.md", "ChildLoser")]:
        path.write_text(marker)

    def skill(path, name, description, disabled=False):
        path.mkdir(parents=True)
        file = path / "SKILL.md"
        file.write_text(f"---\nname: {name}\ndescription: {description}\ndisable-model-invocation: {str(disabled).lower()}\n---\nBody {name}\n")
        return file

    parent_skill = skill(project / ".agents/skills/shared", "shared", "ParentDescription")
    child_skill = skill(child / ".agents/skills/shared", "shared", "ChildDescription")
    hidden = skill(project / ".pi/skills/manual", "manual", "ManualDescription", True)
    broken = skill(project / ".pi/skills/broken", "Bad Name", "invalid")
    listing = "\n".join(cli([], ":skills\n:q\n"))
    for file in (parent_skill, child_skill, hidden, broken):
        assert str(file.resolve()) in listing, listing
    assert "Duplicate skill shared" in listing and "Invalid skill" in listing

    def request(args, text="probe"):
        cli(["--print", "--no-session", *args, text], "")
        (_, payload), = requests
        requests.clear()
        return payload["messages"]

    system = request([])[0]["content"]
    for marker in ["UserBase", "UserAppend", "ParentFallback", "ChildWinner", "ChildDescription"]:
        assert system.count(marker) == 1, system
    for marker in ["TrustedBase", "ChildLoser", "ChildFallback", "ParentDescription", "ManualDescription", "Body shared"]:
        assert marker not in system, system
    trusted = request(["--trust-project"])[0]["content"]
    assert trusted.count("TrustedBase") == trusted.count("TrustedAppend") == 1
    assert "UserBase" not in trusted and "UserAppend" not in trusted
    system = request(["--trust-project", "--system-prompt", "CliBase", "--append-system-prompt", "CliFirst",
                      "--append-system-prompt", "CliSecond"])[0]["content"]
    ordered = ["CliBase", "ParentFallback", "ChildWinner", "TrustedAppend", "CliFirst", "CliSecond"]
    assert all(system.count(v) == 1 for v in ordered)
    assert [system.index(v) for v in ordered] == sorted(system.index(v) for v in ordered)
    assert "TrustedBase" not in system
    cli([], ":skill:manual apply\n:q\n")
    (_, payload), = requests
    requests.clear()
    assert payload["messages"][-1]["content"].count("Body manual") == 1

    for base in (state, local):
        (base / "prompts").mkdir()
    template = local / "prompts/args.md"
    template.write_text("---\ndescription: quoted args\n---\n$1|$2|\u0024{3:-none}|\u0024{@:2:1}|$ARGUMENTS")
    args = '/args "one two" three'
    assert request(["--trust-project"], args)[1:] == [{"role":"user", "content":"one two|three|none|three|one two three"}]
    assert request([], args)[-1]["content"] == args
    (state / "prompts/args.md").write_text("User template $1")
    assert request(["--trust-project"], args)[-1]["content"] == "User template one two"
    assert "/args" in "\n".join(cli([], ":prompts\n:q\n"))
    cli([], args + "\n:q\n")
    (_, payload), = requests
    requests.clear()
    assert payload["messages"][-1]["content"] == "User template one two"

    result = cli(["--mode", "json", "--keep-recent-tokens", "555", "ephemeral setting"], "")
    sid = next(json.loads(line)["session_id"] for line in result if line.startswith("{") and '"type":"done"' in line)
    assert saved_session(sid).get("keepRecentTokens") != 555
    requests.clear()


    secret = "credential-in-session-97bb"
    (state / "secret-fixture.key").write_text(secret)
    cli(["--print"], secret)
    requests.clear()
    lines = cli(["--continue"], ":diagnostics\n:bug-report\n:changelog 0.1.20\n:q\n")
    assert any("allowlist-aware tool discovery" in line for line in lines)
    for prefix in ["diagnostics-", "bug-report-"]:
        bundle, = state.glob(prefix + "*.json")
        contents = bundle.read_text()
        report = json.loads(contents)
        assert report["session"]["messages"] >= 2
        assert secret not in contents and str(root) not in contents and "secret-fixture" not in contents
        assert bundle.stat().st_mode & 0o077 == 0
        assert ("reproduction" in report) == (prefix == "bug-report-")
    assert requests == []
