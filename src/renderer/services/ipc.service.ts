import { defaultIfEmpty, share } from "rxjs/operators";
import { Observable, ReplaySubject, identity } from "rxjs";
import { IpcRequest } from "shared/models/ipc";
import { deserializeError } from 'serialize-error';
import { IpcChannels, IpcRequestType, IpcResponseType } from "shared/models/ipc/ipc-routes";
import { IpcStreamMessage } from "shared/models/ipc/ipc-stream-message.interface";

export class IpcService {
    private static instance: IpcService;

    public static getInstance(): IpcService {
        if (!IpcService.instance) {
            IpcService.instance = new IpcService();
        }
        return IpcService.instance;
    }

    private constructor() {}

    public sendLazy<T = unknown>(channel: string, request?: IpcRequest<T>): void {
        window.desktop.ipcRenderer.sendLazy(channel, request?.args);
    }

    public sendV2<C extends IpcChannels>(channel: C, data?: IpcRequestType<C>, defaultValue?: IpcResponseType<C>): Observable<IpcResponseType<C>> {
        const requestId = crypto.randomUUID();

        const obs = new Observable<IpcResponseType<C>>(observer => {
            let settled = false;
            const onEvent = (message: IpcStreamMessage<IpcResponseType<C>>) => {
                if (settled) return;
                if (message.kind === "value") {
                    observer.next(message.payload);
                } else if (message.kind === "error") {
                    settled = true;
                    observer.error(deserializeError(message.payload));
                } else if (message.kind === "complete") {
                    settled = true;
                    observer.complete();
                } else {
                    settled = true;
                    observer.error(new Error("Invalid IPC stream message"));
                }
            };

            void window.desktop.ipcRenderer.request(requestId, channel as string, data, onEvent)
                .catch(error => {
                    if (!settled) {
                        settled = true;
                        observer.error(deserializeError(error));
                    }
                });

            return () => {
                if (!settled) {
                    settled = true;
                    void window.desktop.ipcRenderer.cancelRequest(requestId).catch(console.error);
                }
            };
        }).pipe(defaultValue ? defaultIfEmpty(defaultValue) : identity, share({connector: () => new ReplaySubject(1)}));

        return obs;
    }
}
