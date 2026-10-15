import { Observable } from "rxjs";

export interface IpcRequest<T> {
    args?: T;
}

export type IpcReplier<T> = (data: Observable<T>) => void;
