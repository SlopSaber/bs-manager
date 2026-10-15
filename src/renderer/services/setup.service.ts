import { lastValueFrom } from "rxjs";
import { logRenderError } from "renderer";

import { BSVersionManagerService } from "./bs-version-manager.service";
import { InstallationLocationService } from "./installation-location.service";
import { IpcService } from "./ipc.service";
import { ModalService } from "./modale.service";

import { AskInstallPathModal } from "renderer/components/modal/modal-types/setup/ask-install-path.component";
import { PreviewSettingsImportModal } from "renderer/components/modal/modal-types/setup/preview-settings-import-modal.component";
import { ChooseProtonFolderModal } from "renderer/components/modal/modal-types/setup/choose-proton-folder-modal.component";

// Handle setup modals/prompts, ordering of the modals/prompts may be done here
export class SetupService {
    private static instance: SetupService;

    private readonly installationLocationService: InstallationLocationService;
    private readonly ipcService: IpcService;
    private readonly modalService: ModalService;
    private readonly versionManagerService: BSVersionManagerService;

    private constructor() {
        this.installationLocationService = InstallationLocationService.getInstance();
        this.ipcService = IpcService.getInstance();
        this.modalService = ModalService.getInstance();
        this.versionManagerService = BSVersionManagerService.getInstance();
    }

    public static getInstance(): SetupService {
        if (!SetupService.instance) {
            SetupService.instance = new SetupService();
        }
        return SetupService.instance;
    }

    public async check(): Promise<void> {
        try {
            // NOTE: for modal sequencing
            await this.checkLegacySettingsImport();
            await this.checkInstallationPath();

            if (window.desktop.platform === "linux") {
                await this.checkProtonFolder();
            }
        } catch (error) {
            logRenderError(error);
        }
    }

    private async checkLegacySettingsImport(): Promise<void> {
        try {
            const needed = await lastValueFrom(this.ipcService.sendV2("preview.legacy-settings-import-needed"));
            if (!needed) {
                return;
            }

            const response = await this.modalService.openModal(
                PreviewSettingsImportModal,
                { closable: false }
            );

            if (response.data === "import") {
                await lastValueFrom(this.ipcService.sendV2("preview.import-legacy-settings"));
            } else {
                await lastValueFrom(this.ipcService.sendV2("preview.skip-legacy-settings-import"));
            }
        } catch (error) {
            logRenderError(error);
        }
    }

    private async checkInstallationPath(): Promise<void> {
        try {
            const exists = await lastValueFrom(this.ipcService.sendV2("bs-installer.folder-exists"));
            if (exists) {
                return;
            }

            const modalResponse = await this.modalService.openModal(
                AskInstallPathModal,
                { closable: false }
            );

            await lastValueFrom(this.installationLocationService.setInstallationFolder(modalResponse.data.installPath, false));

            // Refresh the versions tab
            await this.versionManagerService.askInstalledVersions();
        } catch (error) {
            logRenderError(error);
        }
    }

    private async checkProtonFolder(): Promise<void> {
        try {
            const valid = await lastValueFrom(this.ipcService.sendV2("linux.verify-proton-folder"));
            if (valid) {
                return;
            }

            await this.modalService.openModal(
                ChooseProtonFolderModal,
                { closable: false }
            );
        } catch (error) {
            logRenderError(error);
        }
    }

}
