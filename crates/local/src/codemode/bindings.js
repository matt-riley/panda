((invoke, emit, allowed, read, write) => {
    const stringify = JSON.stringify, parse = JSON.parse;
    globalThis.load = key => {
        if (typeof key !== "string") throw new TypeError("load() key must be a string");
        const json = read(key);
        return json === "" ? undefined : parse(json);
    };
    globalThis.store = (key, value) => {
        if (typeof key !== "string") throw new TypeError("store() key must be a string");
        const json = stringify(value);
        if (json === undefined && value !== undefined) throw new TypeError("store() requires a JSON value");
        write(key, json);
    };
    const call = (name, args) => invoke(name, stringify(args));
    const helpers = {
        ls: (args = {}) => parse(call("ls", args)),
        search: args => parse(call("search", args)),
        repo_map: (args = {}) => parse(call("repo_map", args)),
        read: args => call("read", args)
    };
    globalThis.tools = Object.freeze(Object.fromEntries(allowed.map(name => [name, helpers[name]])));
    globalThis.text = value => emit(typeof value === "string" ? value : stringify(value));
})(globalThis.__invoke, globalThis.__text, globalThis.__allowed, globalThis.__load, globalThis.__store);
delete globalThis.__load;
delete globalThis.__store;
delete globalThis.__allowed;
delete globalThis.__invoke;
delete globalThis.__text;
