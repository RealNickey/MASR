import React, { useState, useEffect } from "react";
import { useTranslation } from "react-i18next";
import { getVersion } from "@tauri-apps/api/app";
import { SettingsGroup } from "../../ui/SettingsGroup";
import { SettingContainer } from "../../ui/SettingContainer";
import Badge from "../../ui/Badge";
import { AppLanguageSelector } from "../AppLanguageSelector";
import { RecordingsDirectory } from "../RecordingsDirectory";

export const AboutSettings: React.FC = () => {
  const { t } = useTranslation();
  const [version, setVersion] = useState("");

  useEffect(() => {
    const fetchVersion = async () => {
      try {
        const appVersion = await getVersion();
        setVersion(appVersion);
      } catch (error) {
        console.error("Failed to get app version:", error);
        setVersion("0.8.3");
      }
    };

    fetchVersion();
  }, []);

  return (
    <div className="max-w-xl w-full mx-auto space-y-6">
      <SettingsGroup title="System Preferences &amp; Paths">
        <AppLanguageSelector descriptionMode="tooltip" grouped={true} />
        <SettingContainer
          title={t("settings.about.version.title")}
          description={t("settings.about.version.description")}
          grouped={true}
        >
          <Badge variant="secondary" className="font-mono text-[11px]">
            v{version}
          </Badge>
        </SettingContainer>
        <RecordingsDirectory descriptionMode="tooltip" grouped={true} />
      </SettingsGroup>
    </div>
  );
};
