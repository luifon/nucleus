import { jsonGet, jsonPost, jsonDelete } from "./client";
import type { ImageRow } from "./generated/ImageRow";
import type { GenerateReq } from "./generated/GenerateReq";
import type { StatusResp as GalleryStatus } from "./generated/StatusResp";

// Image-generation (gallery) surface — ADR-019. Talks to the axum
// /gallery/api/* routes, which proxy to a registry of local image-model
// backends (bonsai, noobai) and persist results. PNG bytes are served at
// /gallery/files/<id>.png.
// Wire types are ts-rs-generated from the Rust structs (./generated/).

export type { GenerateReq } from "./generated/GenerateReq";
export type { BackendStatus } from "./generated/BackendStatus";
export type { StatusResp as GalleryStatus } from "./generated/StatusResp";

/** UI-layer refinement: the wire shape (generated ImageRow) carries
 *  `status: string`; this union narrows it to the lifecycle values the
 *  handler actually emits. */
export type ImageStatus = "pending" | "ready" | "failed";

/** Wire shape is generated; `status` narrowing is a UI-layer refinement. */
export type GeneratedImage = Omit<ImageRow, "status"> & {
  status: ImageStatus;
};

/** UI-layer request helper over the generated GenerateReq: the Rust side
 *  deserializes the non-prompt fields as `Option`, so omitting them on the
 *  wire is valid — this relaxes the generated all-required-nullable shape
 *  for callers. */
export type GenerateBody = Pick<GenerateReq, "prompt"> &
  Partial<Omit<GenerateReq, "prompt">>;

export const imageUrl = (id: string) => `/gallery/files/${id}.png`;

export const listImages = () => jsonGet<GeneratedImage[]>("/gallery/api/images");

export const generateImage = (body: GenerateBody) =>
  jsonPost<GeneratedImage, GenerateBody>("/gallery/api/generate", body);

export const deleteImage = (id: string) =>
  jsonDelete<{ ok: boolean; id: string }>(
    `/gallery/api/images/${encodeURIComponent(id)}`,
  );

export const galleryStatus = () => jsonGet<GalleryStatus>("/gallery/api/status");
