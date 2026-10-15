import type { ErrorObject } from "serialize-error";

export interface Progression<T = unknown, D = unknown> {
    total: number;
    current: number;
    diff?: number;
    data?: T;
    extra?: D;
    lastError?: ErrorObject;
}
