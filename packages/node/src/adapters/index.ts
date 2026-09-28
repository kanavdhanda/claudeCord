import type { AdapterId } from "@claudecord/protocol";
import { agy } from "./agy.js";
import { claude } from "./claude.js";
import { codex } from "./codex.js";
import type { Adapter } from "./types.js";

const all: Record<AdapterId, Adapter> = { claude, agy, codex };

export function getAdapter(id: AdapterId): Adapter {
  return all[id];
}

export type { Adapter } from "./types.js";
