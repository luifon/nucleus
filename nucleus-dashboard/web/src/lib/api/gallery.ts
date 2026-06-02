import { jsonGet, jsonPost, jsonDelete } from "./client";

// Image-generation (gallery) surface — ADR-019. Talks to the axum
// /gallery/api/* routes, which proxy to a registry of local image-model
// backends (bonsai, noobai) and persist results. PNG bytes are served at
// /gallery/files/<id>.png.

export type GeneratedImage = {
  id: string;
  prompt: string;
  seed: number;
  width: number;
  height: number;
  steps: number;
  created_at: string;
  model: string;
  status: "pending" | "ready" | "failed";
  error?: string | null;
};

export type GenerateBody = {
  prompt: string;
  model?: string;
  seed?: number;
  steps?: number;
  width?: number;
  height?: number;
};

export type BackendStatus = { name: string; reachable: boolean };
export type GalleryStatus = { backends: BackendStatus[]; default_model: string };

export const imageUrl = (id: string) => `/gallery/files/${id}.png`;

export const listImages = () => jsonGet<GeneratedImage[]>("/gallery/api/images");

export const generateImage = (body: GenerateBody) =>
  jsonPost<GeneratedImage, GenerateBody>("/gallery/api/generate", body);

export const deleteImage = (id: string) =>
  jsonDelete<{ ok: boolean; id: string }>(
    `/gallery/api/images/${encodeURIComponent(id)}`,
  );

export const galleryStatus = () => jsonGet<GalleryStatus>("/gallery/api/status");
