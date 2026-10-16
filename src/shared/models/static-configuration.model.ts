import type { BSVersion } from "../bs-version.interface";
import type { AutoUpdate } from "./config";

export interface StaticConfigKeyValues {
    "installation-folder": string;
    "song-details-cache-etag": string;
    "disable-hadware-acceleration": boolean;
    "use-symlinks": boolean;
    "use-system-proxy": boolean;
    "close-bs-manager-on-launch": boolean;
    "last-version-launched": BSVersion;
    "auto-update": AutoUpdate;

    // Linux-specific setting retained for the existing shared settings contract.
    "proton-folder": string | null;
    "versions": BSVersion[];
}

export type StaticConfigKeys = keyof StaticConfigKeyValues;

export type StaticConfigGetIpcRequestResponse<K extends StaticConfigKeys> = {
    request: K;
    response: StaticConfigKeyValues[K];
};

export type StaticConfigSetIpcRequest<K extends StaticConfigKeys> = {
    request: { key: K; value: StaticConfigKeyValues[K] };
    response: void;
};
