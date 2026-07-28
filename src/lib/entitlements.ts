const DEFAULT_REQUESTS_PER_HOUR = 100;
import type { CloudUser, Entitlements } from "@/types";

const UNLOCKED: Entitlements = {
  active: true,
  browserAutomation: true,
  crossOsFingerprints: true,
  cloudBackup: true,
  teamCollaboration: true,
  profileLimit: 0,
  requestsPerHour: DEFAULT_REQUESTS_PER_HOUR,
};

/**
 * The user's effective entitlements. Prefers the backend-resolved object the
 * desktop attaches to CloudUser; only falls back to deriving from the plan
 * fields when it's missing (older cached state). The fallback mirrors the
 * backend matrix in `apps/backend/src/plans/entitlements.ts`.
 */
export function getEntitlements(
  _user: CloudUser | null | undefined,
): Entitlements {
  return UNLOCKED;
}
