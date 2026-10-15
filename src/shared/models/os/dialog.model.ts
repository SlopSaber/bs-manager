export interface FileFilter {
    name: string;
    extensions: string[];
}

export type OpenDialogProperty =
    | "openFile"
    | "openDirectory"
    | "multiSelections"
    | "showHiddenFiles"
    | "createDirectory"
    | "promptToCreate"
    | "noResolveAliases"
    | "treatPackageAsDirectory"
    | "dontAddToRecent";

export interface OpenDialogOptions {
    title?: string;
    defaultPath?: string;
    buttonLabel?: string;
    filters?: FileFilter[];
    properties?: OpenDialogProperty[];
    message?: string;
}

export interface OpenDialogReturnValue {
    canceled: boolean;
    filePaths: string[];
}

export interface OpenSaveDialogOption {
    filename?: string;
    filters?: FileFilter[];
}
