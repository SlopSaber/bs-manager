import { useEffect, useState } from "react";

export function useWindowArgs<Key extends string>(...keys: Key[]): Record<Key, string|undefined> {

    const getArgs = (...keys: Key[]) => {
        const url = new URLSearchParams(window.location.search);
        const result = {} as Record<Key, string>;
        keys.forEach(key => {
            result[key] = url.get(key) || undefined;
        });

        return result;
    }

    const [args, setArgs] = useState(() => getArgs(...keys));
    const keysSignature = keys.join("\u0000");

    useEffect(() => {
        const unlisten = window.desktop.ipcRenderer.on("window-arguments", payload => {
            if (!payload || typeof payload !== "object") return;
            const values = payload as Record<string, unknown>;
            setArgs(Object.fromEntries(keys.map(key => {
                const value = values[key];
                return [key, value === undefined || value === null ? undefined : String(value)];
            })) as Record<Key, string|undefined>);
        });
        return () => unlisten?.();
    }, [keysSignature]);

    return args;
}
