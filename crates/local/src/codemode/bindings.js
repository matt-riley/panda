((invoke, emit, allowed) => {
    const stringify = JSON.stringify, parse = JSON.parse;
    const call = (name, args) => invoke(name, stringify(args));
    const helpers = {
        ls: (args = {}) => parse(call("ls", args)),
        search: args => parse(call("search", args)),
        repo_map: (args = {}) => parse(call("repo_map", args)),
        read: args => call("read", args)
    };
    globalThis.tools = Object.freeze(Object.fromEntries(allowed.map(name => [name, helpers[name]])));
    globalThis.text = value => emit(typeof value === "string" ? value : stringify(value));
})(globalThis.__invoke, globalThis.__text, globalThis.__allowed);
delete globalThis.__allowed;
delete globalThis.__invoke;
delete globalThis.__text;
