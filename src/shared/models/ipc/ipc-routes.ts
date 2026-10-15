import type { Progression } from "../progression.model";
import type { Observable } from "rxjs";
import type { BSVersion } from "shared/bs-version.interface";
import type { BSLaunchEventData, LaunchOption } from "shared/models/bs-launch";
import type { BsvMapDetail, SongDetails } from "shared/models/maps";
import type { BsmLocalMap, BsmLocalMapsProgress, DeleteMapsProgress } from "shared/models/maps/bsm-local-map.interface";
import type { BsvPlaylist, BsvPlaylistPage, PlaylistSearchParams, SearchParams } from "../maps/beat-saver.model";
import type { ImportVersionOptions } from "../bs-version-import.model";
import type { DownloadSteamInfo, OculusDownloadInfo } from "../bs-version-download/download-info.model";
import type { DepotDownloaderEvent } from "../bs-version-download/depot-downloader.model";
import type { MSGetQuery, MSModel, MSModelType } from "../models/model-saber.model";
import type { ModelDownload } from "../models/model-download.model";
import type { BsmLocalModel } from "../models/bsm-local-model.interface";
import type { BPList, DownloadPlaylistProgressionData } from "../playlists/playlist.interface";
import type { VersionLinkerAction } from "../version-linker.model";
import type { FileFilter, OpenDialogOptions, OpenDialogReturnValue } from "../os/dialog.model";
import type { SystemNotificationOptions } from "../notification/system-notification.model";
import type { Supporter } from "../supporters";
import type { AppWindow } from "../window-manager/app-window.model";
import type { LocalBPList, LocalBPListsDetails } from "../playlists/local-playlist.models";
import type { StaticConfigGetIpcRequestResponse, StaticConfigKeys, StaticConfigSetIpcRequest } from "../static-configuration.model";
import type { BbmFullMod, BbmModVersion, ExternalMod } from "../mods/mod.interface";
import type { UpdateInfo } from "../update-info.model";
import type { VrRuntime } from "shared/models/vr-runtime.model";

export type IpcReplier<T> = (data: Observable<T>) => void;

export interface IpcChannelMapping {

    /* ** bs-download-ipcs ** */
    "import-version": { request: ImportVersionOptions, response: Progression<BSVersion>};
    "auto-download-bs-version": { request: DownloadSteamInfo, response: DepotDownloaderEvent};
    "download-bs-version": { request: DownloadSteamInfo, response: DepotDownloaderEvent};
    "download-bs-version-qr": { request: DownloadSteamInfo, response: DepotDownloaderEvent};
    "stop-download-bs-version": { request: void, response: void};
    "send-input-bs-download": { request: string, response: boolean};
    "bs-oculus-download": { request: OculusDownloadInfo, response: Progression<BSVersion>};
    "bs-oculus-stop-download": { request: void, response: void};
    "login-with-meta": { request: boolean, response: string };
    "delete-meta-session": { request: void, response: void };
    "meta-session-exists": { request: void, response: boolean };

    /* ** beat-saver-ipcs ** */
    "bsv-search-map": {request: SearchParams, response: BsvMapDetail[]};
    "bsv-get-map-details-from-hashs": {request: string[], response: BsvMapDetail[]};
    "bsv-get-map-details-by-id": {request: string, response: BsvMapDetail};
    "bsv-search-playlist": {request: PlaylistSearchParams, response: BsvPlaylist[]};
    "bsv-get-playlist-details-by-id": {request: {id: string, page: number}, response: BsvPlaylistPage};

    /* ** bs-installer-ipcs ** */
    "bs-installer.folder-exists": { request: void, response: boolean };
    "bs-installer.default-install-path": { request: void, response: string };
    "bs-installer.install-path": { request: void, response: string};
    "bs-installer.set-install-path": { request: { path: string, move: boolean }, response: string};
    "preview.legacy-settings-import-needed": { request: void, response: boolean };
    "preview.import-legacy-settings": { request: void, response: void };
    "preview.skip-legacy-settings-import": { request: void, response: void };

    /* ** bs-launcher-ipcs ** */
    "create-launch-shortcut": { request: { options: LaunchOption, steamShortcut?: boolean }, response: boolean };
    "bs-launch.need-start-as-admin": { request: void, response: boolean };
    "bs-launch.launch": { request: LaunchOption, response: BSLaunchEventData };

    /* ** bs-maps-ipcs ** */
    "load-version-maps": { request: BSVersion, response: BsmLocalMapsProgress};
    "delete-maps": { request: BsmLocalMap[], response: DeleteMapsProgress };
    "export-maps": { request: { version: BSVersion; maps: BsmLocalMap[]; outPath: string }, response: Progression };
    "bs-maps.import-maps": { request: { paths: string[]; version: BSVersion | undefined }, response: Progression<BsmLocalMap> };
    "bs-maps.download-map": { request: { map: BsvMapDetail; version: BSVersion | undefined }, response: BsmLocalMap };
    "last-downloaded-map": { request: void, response: { version?: BSVersion, map: BsmLocalMap } };
    "one-click-install-map": { request: BsvMapDetail, response: void };
    "register-maps-deep-link": { request: void, response: boolean };
    "unregister-maps-deep-link": { request: void, response: boolean };
    "is-map-deep-links-enabled": { request: void, response: boolean };
    "get-maps-info-from-cache": { request: string[], response: SongDetails[] }

    /* ** bs-model-ipcs ** */
    "one-click-install-model": { request: MSModel, response: void };
    "register-models-deep-link": { request: void, response: boolean };
    "unregister-models-deep-link": { request: void, response: boolean };
    "is-models-deep-links-enabled": { request: void, response: boolean };
    "download-model": { request: ModelDownload, response: Progression<BsmLocalModel> };
    "get-version-models": { request: { version: BSVersion; type: MSModelType }, response: Progression<BsmLocalModel[]> };
    "export-models": { request: { version: BSVersion; models: BsmLocalModel[]; outPath: string }, response: Progression };
    "delete-models": { request: BsmLocalModel[], response: Progression<BsmLocalModel[]> };

    /* ** bs-mods-ipcs ** */
    "bs-mods.get-available-mods": { request: BSVersion, response: BbmFullMod[] };
    "bs-mods.get-installed-mods": { request: BSVersion, response: BbmModVersion[] };
    "bs-mods.import-mods": { request: { paths: string[]; version: BSVersion; }, response: Progression<ExternalMod> };
    "bs-mods.install-mods": { request: { mods: BbmFullMod[]; version: BSVersion }, response: Progression };
    "bs-mods.uninstall-mods": { request: { mods: BbmFullMod[]; version: BSVersion }, response: Progression };
    "bs-mods.uninstall-all-mods": { request: BSVersion, response: Progression };
    "bs-mods.beatmods-up": { request: void, response: boolean };

    /* ** bs-playlist-ipcs ** */
    "one-click-install-playlist": { request: string, response: Progression<DownloadPlaylistProgressionData> };
    "register-playlists-deep-link": { request: void, response: boolean };
    "unregister-playlists-deep-link": { request: void, response: boolean };
    "is-playlists-deep-links-enabled": { request: void, response: boolean };
    "download-playlist": {request: {downloadSource: string, dest?: string, version?: BSVersion, ignoreSongsHashs?: string[]}, response: Progression<DownloadPlaylistProgressionData>};
    "get-version-playlists-details": {request: BSVersion, response: Progression<LocalBPListsDetails[]>};
    "delete-playlist": {request: {version: BSVersion, bpList: LocalBPList, deleteMaps?: boolean}, response: Progression};
    "export-playlists": {request: {version?: BSVersion, bpLists: LocalBPList[], dest: string, playlistsMaps?: BsmLocalMap[]}, response: Progression<string>};
    "import-playlists": {request: {version?: BSVersion, paths: string[]}, response: Progression<LocalBPListsDetails>};
    "install-playlist-file": {request: {bplist: BPList, version?: BSVersion, dest?: string}, response: LocalBPListsDetails};

    /* ** bs-uninstall-ipcs ** */
    "bs.uninstall": { request: BSVersion, response: void };

    /* ** bs-version-ipcs ** */
    "bs-version.get-version-dict": { request: { refresh?: boolean }, response: BSVersion[] };
    "bs-version.installed-versions": { request: void, response: BSVersion[] };
    "bs-version.open-folder": { request: BSVersion, response: void };
    "bs-version.edit": { request: { version: BSVersion; name: string; color: string }, response: BSVersion };
    "bs-version.clone": { request: { version: BSVersion; name: string; color: string }, response: BSVersion };
    "get-version-full-path": { request: BSVersion, response: string };
    "full-version-path-to-relative": { request: { version: BSVersion; fullPath: string }, response: string };
    "get-linked-folders": { request: { version: BSVersion; options?: { relative?: boolean } }, response: string[] };
    "link-version-folder-action": { request: VersionLinkerAction, response: void };
    "is-version-folder-linked": { request: { version: BSVersion; relativeFolder: string }, response: boolean };
    "relink-all-versions-folders": { request: void, response: void };
    "get-shared-folder": { request: void, response: string };

    /* ** launcher-ipcs ** */
    "download-update": { request: void, response: Progression };
    "check-update": { request: void, response: boolean };
    "get-available-update": { request: void, response: UpdateInfo | null };
    "install-update": { request: void, response: void };

    /* ** model-saber.ipcs ** */
    "ms-get-model-by-id": { request: string | number, response: MSModel };
    "search-models": { request: MSGetQuery, response: MSModel[] };

    /* ** os-controls-ipcs ** */
    "new-window": { request: string, response: void };
    "open-dialog": { request: OpenDialogOptions, response: OpenDialogReturnValue };
    "choose-folder": { request: { defaultPath?: string, parent?: "home", showHidden?: boolean }, response: OpenDialogReturnValue };
    "choose-file": { request: string, response: OpenDialogReturnValue }
    "choose-image": { request: { multiple?: boolean, base64?: boolean }, response: string[] }
    "window.progression": { request: number, response: void };
    "save-file": { request: { filename?: string; filters?: FileFilter[] }, response: string };
    "current-version": { request: void, response: string };
    "open-logs": { request: void, response: string };
    "notify-system": { request: SystemNotificationOptions, response: void };
    "view-path-in-explorer": { request: string, response: void };
    "restart-app": { request: void, response: void };

    /* ** supporters-ipcs ** */
    "get-supporters": { request: void, response: Supporter[] };

    /* **window-manager-ipcs ** */
    "close-window": { request: void, response: void };
    "maximise-window": { request: void, response: void };
    "minimise-window": { request: void, response: void };
    "unmaximise-window": { request: void, response: void };
    "open-window-then-close-all": { request: AppWindow, response: void };
    "open-window-or-focus": { request: AppWindow, response: void };

    /* ** static-configuration.ipcs ** */
    "static-configuration.get": StaticConfigGetIpcRequestResponse<StaticConfigKeys>;
    "static-configuration.set": StaticConfigSetIpcRequest<StaticConfigKeys>;
    "static-configuration.delete": { request: StaticConfigKeys; response: void };

    /* ** linux.ipcs ** */
    "linux.set-proton-folder": { request: string, response: boolean };
    "linux.verify-proton-folder": { request: string | void, response: boolean };
    "linux.get-wine-prefix-path": { request: void, response: string };

    /* ** oculus.ipcs ** */
    "is-oculus-sideloaded-apps-enabled": { request: void, response: boolean };
    "enable-oculus-sideloaded-apps": { request: void, response: void };

    /* ** vr-runtime.ipcs ** */
    "vr-runtime.get-active": { request: string | void, response: VrRuntime };

    /* ** OTHERS (if your IPC channel is not in a "-ipcs" file, put it here) ** */
    "shortcut-launch-options": { request: void, response: LaunchOption };
}

export type IpcRequestType<Channel extends keyof IpcChannelMapping> = IpcChannelMapping[Channel]['request'];
export type IpcResponseType<Channel extends keyof IpcChannelMapping> = IpcChannelMapping[Channel]['response'];
export type IpcChannels = keyof IpcChannelMapping;
