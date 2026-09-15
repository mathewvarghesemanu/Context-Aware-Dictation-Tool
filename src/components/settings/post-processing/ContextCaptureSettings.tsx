import React, { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";

import { commands } from "../../../bindings";
import { Alert } from "../../ui/Alert";
import { ToggleSwitch } from "../../ui/ToggleSwitch";
import { useSettings } from "../../../hooks/useSettings";
import { useOsType } from "../../../hooks/useOsType";

/**
 * Where the screenshot toggle is in the permission handshake.
 *
 * - `idle`: nothing in flight; the toggle reflects the stored setting.
 * - `requesting`: the system prompt is up and we are waiting on the answer.
 * - `denied`: the user never granted it (or the grant needs a restart to
 *   take effect); flipping the switch again starts over.
 * - `revoked`: the setting was on but the permission is gone, so it was
 *   switched back off.
 */
type PermissionPhase = "idle" | "requesting" | "denied" | "revoked";

/**
 * Cursor-context capture for post-processing: read the text around the caret
 * so the model can get capitalization and sentence continuation right, with an
 * opt-in screenshot fallback for apps that expose no accessible text.
 *
 * The screenshot toggle is nested under (and disabled by) the text toggle —
 * it is a fallback for that capture, not an independent feature.
 *
 * The screenshot fallback is useless without the Screen Recording permission,
 * so the toggle owns that handshake: turning it on raises the system prompt,
 * and the setting is only persisted once the permission actually reads as
 * granted.
 *
 * The permission is **never polled**. `CGPreflightScreenCaptureAccess` is a
 * WindowServer RPC, and Handy holds an active CGEventTap for its global
 * shortcuts, so a repeating check stalls input across the whole machine (the
 * same failure as cjpais/Handy#1827). Every check here is tied to a discrete
 * event: opening the settings, the prompt returning, the window regaining
 * focus after a trip to System Settings, or the explicit "check again" button.
 */
const ContextCaptureSettingsComponent: React.FC = () => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating } = useSettings();
  const osType = useOsType();

  const supported = osType === "macos";
  const captureEnabled =
    (getSetting("context_capture_enabled") ?? false) && supported;
  const screenshotEnabled =
    (getSetting("context_capture_screenshot_enabled") ?? false) &&
    captureEnabled;

  const [phase, setPhase] = useState<PermissionPhase>("idle");
  const [checking, setChecking] = useState(false);

  const unmountedRef = useRef(false);
  useEffect(() => {
    unmountedRef.current = false;
    return () => {
      unmountedRef.current = true;
    };
  }, []);

  /**
   * One permission check, persisting the setting when the answer is yes.
   * Returns the answer so callers can pick their own failure phase.
   */
  const checkAndPersist = useCallback(async (): Promise<boolean> => {
    let granted = false;
    try {
      granted = await commands.checkScreenCapturePermission();
    } catch {
      return false;
    }

    if (granted && !unmountedRef.current) {
      await updateSetting("context_capture_screenshot_enabled", true);
      if (!unmountedRef.current) setPhase("idle");
    }
    return granted;
  }, [updateSetting]);

  // A permission granted in a past session can be taken away in System
  // Settings, which would leave the toggle on while the capture silently does
  // nothing. Turn it back off so the switch never claims more than it can do.
  useEffect(() => {
    if (!supported || !screenshotEnabled) return;

    let cancelled = false;
    commands
      .checkScreenCapturePermission()
      .then(async (granted) => {
        if (cancelled || granted) return;
        await updateSetting("context_capture_screenshot_enabled", false);
        if (!cancelled) setPhase("revoked");
      })
      .catch(() => {
        // Permission state is unknown — leave the setting alone rather than
        // switching off something that may well be working.
      });

    return () => {
      cancelled = true;
    };
    // Runs on mount and whenever the toggle turns on; the handshake below
    // already checked the permission in that case, and the backend answers
    // repeats from a short-lived cache, so this costs nothing.
  }, [supported, screenshotEnabled, updateSetting]);

  // Granting in System Settings means leaving and coming back to Handy. That
  // return is the signal to re-check — one check per trip, instead of a timer.
  useEffect(() => {
    if (!supported) return;
    if (phase !== "denied" && phase !== "revoked") return;

    const onFocus = () => {
      void checkAndPersist();
    };
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [supported, phase, checkAndPersist]);

  const handleRecheck = useCallback(async () => {
    setChecking(true);
    try {
      const granted = await checkAndPersist();
      if (!granted && !unmountedRef.current) setPhase("denied");
    } finally {
      if (!unmountedRef.current) setChecking(false);
    }
  }, [checkAndPersist]);

  const handleScreenshotToggle = useCallback(
    async (enabled: boolean) => {
      if (!enabled) {
        setPhase("idle");
        await updateSetting("context_capture_screenshot_enabled", false);
        return;
      }

      // Ask for Screen Recording at the moment the user opts in, rather than
      // surprising them with a system prompt mid-dictation. The setting is
      // only written once the permission reads as granted, so the switch
      // cannot sit in the on position without the access behind it.
      if (await checkAndPersist()) return;

      setPhase("requesting");

      // The backend raises the prompt on the main thread with the global
      // keyboard hook torn down, and resolves once the user has answered.
      const result = await commands.requestScreenCapturePermission();
      if (unmountedRef.current) return;

      if (result.status === "error" || !result.data) {
        setPhase("denied");
        return;
      }

      await updateSetting("context_capture_screenshot_enabled", true);
      if (!unmountedRef.current) setPhase("idle");
    },
    [checkAndPersist, updateSetting],
  );

  return (
    <>
      {!supported && (
        <Alert variant="info" contained>
          {t("settings.postProcessing.context.unsupportedPlatform")}
        </Alert>
      )}

      <ToggleSwitch
        checked={captureEnabled}
        onChange={(enabled) =>
          updateSetting("context_capture_enabled", enabled)
        }
        isUpdating={isUpdating("context_capture_enabled")}
        disabled={!supported}
        label={t("settings.postProcessing.context.capture.title")}
        description={t("settings.postProcessing.context.capture.description")}
        descriptionMode="tooltip"
        grouped={true}
      />

      <div className="pl-6">
        <ToggleSwitch
          checked={screenshotEnabled}
          onChange={handleScreenshotToggle}
          isUpdating={
            isUpdating("context_capture_screenshot_enabled") ||
            phase === "requesting"
          }
          disabled={!captureEnabled}
          label={t("settings.postProcessing.context.screenshot.title")}
          description={t(
            "settings.postProcessing.context.screenshot.description",
          )}
          descriptionMode="tooltip"
          grouped={true}
        />
      </div>

      {captureEnabled && phase === "requesting" && (
        <Alert variant="info" contained>
          {t("settings.postProcessing.context.screenshot.permissionWaiting")}
        </Alert>
      )}

      {captureEnabled && (phase === "denied" || phase === "revoked") && (
        <Alert variant="warning" contained>
          <div className="flex flex-col items-start gap-2">
            <span>
              {phase === "denied"
                ? t(
                    "settings.postProcessing.context.screenshot.permissionRequired",
                  )
                : t(
                    "settings.postProcessing.context.screenshot.permissionRevoked",
                  )}
            </span>
            <button
              type="button"
              onClick={handleRecheck}
              disabled={checking}
              className="underline underline-offset-2 disabled:opacity-50"
            >
              {t(
                "settings.postProcessing.context.screenshot.permissionRecheck",
              )}
            </button>
          </div>
        </Alert>
      )}
    </>
  );
};

export const ContextCaptureSettings = React.memo(
  ContextCaptureSettingsComponent,
);
ContextCaptureSettings.displayName = "ContextCaptureSettings";
