import type { BSVersion } from "../bs-version.interface";

export interface LinkOptions {
    keepContents?: boolean;
    intermediateFolder?: string;
    backup?: boolean;
}

export interface UnlinkOptions extends LinkOptions {
    moveContents?: boolean;
}

export enum VersionLinkerActionType {
    Link = "link",
    Unlink = "unlink",
}

export interface VersionLinkerAction {
    version: BSVersion;
    relativeFolder: string;
    type: VersionLinkerActionType;
    options?: LinkOptions;
}

export type VersionLinkFolderAction = Omit<VersionLinkerAction, "type">;
export type VersionUnlinkFolderAction = Omit<VersionLinkerAction, "type"> & { options: UnlinkOptions };
