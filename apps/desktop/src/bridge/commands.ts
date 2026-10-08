// Typed wrappers over Tauri `invoke`. Each name matches a
// `#[tauri::command]` in src-tauri/src/commands.rs; argument keys are the
// camelCase forms Tauri derives from the Rust parameter names.

import { invoke as tauriInvoke, isTauri } from "@tauri-apps/api/core";

import { mockInvoke } from "./mock";
import type {
  AccountSummary,
  CmdError,
  GuestAccess,
  HostAllocation,
  HostSnapshot,
  Images,
  Instance,
  Offer,
  OfferQuery,
  PeerInfo,
  PeerOverview,
  Topology,
  VmSpec,
  WebhookOutcome,
} from "./types";

export const inTauri = isTauri();

export class BridgeError extends Error {
  readonly code: CmdError["code"];
  constructor(e: CmdError) {
    super(e.message);
    this.code = e.code;
  }
}

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return inTauri ? await tauriInvoke<T>(cmd, args) : await mockInvoke<T>(cmd, args);
  } catch (e) {
    if (e && typeof e === "object" && "code" in e && "message" in e) throw new BridgeError(e as CmdError);
    throw new BridgeError({ code: "internal", message: String(e) });
  }
}

export const commands = {
  getHostSnapshot: () => call<HostSnapshot>("get_host_snapshot"),
  setHostAllocation: (allocation: HostAllocation) => call<HostAllocation>("set_host_allocation", { allocation }),
  listOffers: (query: OfferQuery = {}) => call<Offer[]>("list_offers", { query }),
  deployInstance: (offerId: string, spec: VmSpec) => call<Instance>("deploy_instance", { request: { offerId, spec } }),
  listInstances: () => call<Instance[]>("list_instances"),
  scaleInstance: (id: string, replicas: 0 | 1) => call<Instance>("scale_instance", { id, replicas }),
  terminateInstance: (id: string) => call<Instance>("terminate_instance", { id }),
  getWallet: () => call<AccountSummary>("get_wallet"),
  topUp: (amount: number) => call<WebhookOutcome>("top_up", { amount }),
  getTopology: () => call<Topology>("get_topology"),
  killPeer: (peer: string) => call<Topology>("kill_peer", { peer }),
  restorePeer: (peer: string) => call<Topology>("restore_peer", { peer }),
  getInstanceAccess: (id: string) => call<GuestAccess | null>("get_instance_access", { id }),
  getConsole: (id: string) => call<string | null>("get_console", { id }),
  openGuestScreen: (id: string) => call<void>("open_guest_screen", { id }),
  listImages: () => call<Images>("list_images"),
  pullImage: (name: string) => call<void>("pull_image", { name }),
  /** Opens a file picker; resolves to the new image's name, or null when cancelled. */
  importImage: () => call<string | null>("import_image"),
  getPeers: () => call<PeerOverview>("get_peers"),
  /** `pv-…@host:port` (an invite) or `host[:port]`. */
  addPeer: (address: string) => call<PeerInfo>("add_peer", { address }),
  approvePeer: (id: string) => call<PeerInfo>("approve_peer", { id }),
  removePeer: (id: string) => call<void>("remove_peer", { id }),
};
