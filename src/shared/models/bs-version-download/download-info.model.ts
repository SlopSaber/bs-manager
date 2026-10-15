import type { BSVersion } from "../../bs-version.interface";

export interface DownloadInfo {
    bsVersion: BSVersion;
    isVerification?: boolean;
    stay?: boolean;
}

export interface DownloadSteamInfo extends DownloadInfo {
    username?: string;
    password?: string;
}

export interface OculusDownloadInfo extends DownloadInfo {
    token: string;
}
