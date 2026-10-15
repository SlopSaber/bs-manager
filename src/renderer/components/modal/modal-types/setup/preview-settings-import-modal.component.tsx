import { useTranslation } from "renderer/hooks/use-translation.hook";
import { ModalComponent, ModalExitCode } from "renderer/services/modale.service";
import { BsmButton } from "renderer/components/shared/bsm-button.component";

export const PreviewSettingsImportModal: ModalComponent<"import" | "skip", {}> = ({ resolver }) => {
    const t = useTranslation();

    return (
        <div className="max-w-xl w-max">
            <h1 className="tracking-wide w-full uppercase text-3xl text-center">
                {t("modals.preview-settings-import.title")}
            </h1>
            <p className="py-3">
                {t("modals.preview-settings-import.description")}
            </p>
            <div className="h-8 grid grid-flow-col grid-cols-2 gap-2">
                <BsmButton
                    typeColor="cancel"
                    className="rounded-md text-center transition-all"
                    onClick={() => resolver({ exitCode: ModalExitCode.COMPLETED, data: "skip" })}
                    withBar={false}
                    text="modals.preview-settings-import.start-fresh"
                />
                <BsmButton
                    typeColor="primary"
                    className="rounded-md text-center transition-all"
                    onClick={() => resolver({ exitCode: ModalExitCode.COMPLETED, data: "import" })}
                    withBar={false}
                    text="modals.preview-settings-import.import-settings"
                />
            </div>
        </div>
    );
};
