declare global {
    interface Window {
        desktop: {
            platform: "win32";
            ipcRenderer: {
                sendMessage(channel: string, args?: unknown): void;
                on(channel: string, func: (...args: unknown[]) => void): (() => void) | undefined;
                once(channel: string, func: (...args: unknown[]) => void): void;
                removeAllListeners(channel: string): void;
            };
            path: {
                sep: "\\";
                basename(path: string): string;
                join(...parts: string[]): string;
            };
            webUtils: {
                getPathForFile(file: File): string;
            };
        };
    }
}

export {};
