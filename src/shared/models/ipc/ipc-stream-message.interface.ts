export type IpcStreamMessage<T = unknown> =
    | { kind: "value"; payload: T }
    | { kind: "complete" }
    | { kind: "error"; payload: unknown };
