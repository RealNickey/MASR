import { HardDriveDownload, LoaderCircle } from "lucide-react";
import { useTranslation } from "react-i18next";
import type { InitialSetupStatus } from "@/bindings";

interface InitialSetupEmptyStateProps {
  status: InitialSetupStatus;
}

const formatBytes = (bytes: number) => {
  if (bytes < 1024 * 1024 * 1024) {
    return `${Math.max(0, Math.round(bytes / (1024 * 1024)))} MB`;
  }
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GB`;
};

export function InitialSetupEmptyState({
  status,
}: InitialSetupEmptyStateProps) {
  const { t } = useTranslation();
  const percentage =
    status.total > 0
      ? Math.min(100, Math.round((status.downloaded / status.total) * 100))
      : 0;
  const isStorageError = status.phase === "insufficient_storage";

  const phaseText = isStorageError
    ? t("initialSetup.insufficientStorage")
    : t(`initialSetup.phases.${status.phase}`);

  return (
    <section className="mx-auto flex min-h-[420px] max-w-xl flex-col items-center justify-center px-6 text-center">
      <div className="mb-6 flex h-16 w-16 items-center justify-center rounded-2xl bg-forest-green/10 text-forest-green">
        {isStorageError ? (
          <HardDriveDownload className="h-8 w-8" />
        ) : (
          <LoaderCircle className="h-8 w-8 animate-spin" />
        )}
      </div>
      <h2 className="text-xl font-bold text-charcoal">
        {t("initialSetup.title")}
      </h2>
      <p className="mt-2 max-w-md text-sm leading-6 text-bark-grey">
        {isStorageError
          ? t("initialSetup.storageDescription", {
              required: formatBytes(status.required_bytes),
              available: formatBytes(status.available_bytes ?? 0),
            })
          : t("initialSetup.description")}
      </p>

      {!isStorageError && (
        <div className="mt-8 w-full max-w-md">
          <div className="mb-2 flex items-center justify-between text-xs font-medium text-bark-grey">
            <span>{phaseText}</span>
            <span>{percentage}%</span>
          </div>
          <div className="h-2 overflow-hidden rounded-full bg-stone-mist/70">
            <div
              className="h-full rounded-full bg-forest-green transition-[width] duration-300"
              style={{ width: `${percentage}%` }}
            />
          </div>
          {status.total > 0 && (
            <p className="mt-3 text-xs text-bark-grey">
              {t("initialSetup.transfer", {
                downloaded: formatBytes(status.downloaded),
                total: formatBytes(status.total),
              })}
            </p>
          )}
        </div>
      )}
    </section>
  );
}
