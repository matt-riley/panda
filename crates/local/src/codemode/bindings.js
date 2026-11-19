((invoke, emit) => {
    const stringify = JSON.stringify, parse = JSON.parse;
    const call = (name, args) => invoke(name, stringify(args));
    globalThis.tools = Object.freeze({
        ls: (args = {}) => parse(call("ls", args)),
        search: args => parse(call("search", args)),
        repo_map: (args = {}) => parse(call("repo_map", args)),
        read: args => call("read", args)
    });
    globalThis.text = value => emit(typeof value === "string" ? value : stringify(value));
})(globalThis.__invoke, globalThis.__text);
delete globalThis.__invoke;
delete globalThis.__text;
