import { convertFileSrc } from "@tauri-apps/api/core";

export function toTauriAssetUrl(source?: string): string | undefined {
    if (!source?.startsWith("file://")) {
        return source;
    }

    try {
        const fileUrl = new URL(source);
        if (fileUrl.protocol !== "file:") {
            return source;
        }

        let path = decodeURIComponent(fileUrl.pathname).replace(/\//g, "\\");
        if (/^\\?[a-z]:\\/i.test(path)) {
            path = path.replace(/^\\/, "");
        }
        if (fileUrl.hostname) {
            path = `\\\\${fileUrl.hostname}${path}`;
        }

        return convertFileSrc(path);
    } catch {
        return source;
    }
}
