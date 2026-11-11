import wasm_init, {
    setup,
    DesktopSize,
    DeviceEvent,
    InputTransaction,
    SessionBuilder,
    ClipboardData,
    Extension,
    RdpFile,
} from '../../../crates/ironrdp-web/pkg/ironrdp_web';

export async function init(log_level: string) {
    await wasm_init();
    setup(log_level);
}

export { RdpFile };

export const Backend = {
    DesktopSize: DesktopSize,
    InputTransaction: InputTransaction,
    SessionBuilder: SessionBuilder,
    ClipboardData: ClipboardData,
    DeviceEvent: DeviceEvent,
};

// --- Pre-connection configuration extensions ---

export function preConnectionBlob(pcb: string): Extension {
    return new Extension('pcb', pcb);
}

export type VmConnectMode = 'enhanced' | 'basic';

export function vmConnect(vmId: string, mode: VmConnectMode = 'enhanced'): Extension {
    if (vmId.trim() === '') {
        throw new Error('vmconnect requires a VM ID');
    }

    const payload = mode === 'enhanced' ? `${vmId};EnhancedMode=1` : vmId;
    return new Extension('vmconnect', payload);
}

export function displayControl(enable: boolean): Extension {
    return new Extension('display_control', enable);
}

export function kdcProxyUrl(url: string): Extension {
    return new Extension('kdc_proxy_url', url);
}

export function outboundMessageSizeLimit(limit: number): Extension {
    return new Extension('outbound_message_size_limit', limit);
}

export function enableCredssp(enable: boolean): Extension {
    return new Extension('enable_credssp', enable);
}

/**
 * Allow unencrypted Standard RDP Security over a trusted RDCleanPath transport.
 *
 * Enabling this disables TLS and CredSSP protocol negotiation. Use it only when
 * the WebSocket and proxy-to-server transports already provide the required
 * confidentiality and peer authentication.
 */
export function enableStandardRdpSecurity(enable: boolean): Extension {
    return new Extension('enable_standard_rdp_security', enable);
}

export function enableServerPointer(enable: boolean): Extension {
    return new Extension('enable_server_pointer', enable);
}

export function legacyGraphics(enable: boolean): Extension {
    return new Extension('legacy_graphics', enable);
}

// --- Graphics Pipeline (MS-RDPEGFX) and H.264 ---

/**
 * Uses the Graphics Pipeline: the server sends RemoteFX Progressive, ClearCodec and planar
 * updates and, with an H.264 decoder (see `h264Decoder`), H.264 (AVC420 and AVC444).
 */
export function graphicsPipeline(enable: boolean): Extension {
    return new Extension('graphics_pipeline', enable);
}

/** The YUV 4:2:0 planes of one decoded H.264 picture. */
export interface H264Picture {
    width: number;
    height: number;
    y: Uint8Array;
    u: Uint8Array;
    v: Uint8Array;
    /** Bytes per luma row (at least `width`). */
    yStride: number;
    /** Bytes per chroma row (at least `width / 2`, rounded up). */
    uvStride: number;
}

/** Decodes one surface's H.264 stream (Annex B byte stream, or AVC length-prefixed NAL units). */
export interface H264Decoder {
    /** Resolves with the picture `data` carries; `data` carries exactly one picture. */
    decode(data: Uint8Array): Promise<H264Picture>;
    close(): void;
}

/**
 * Lets the Graphics Pipeline decode H.264 with the page's decoder (for example WebCodecs).
 * `factory` is called once for each surface stream. Only give it when the browser can
 * decode H.264: the client then advertises the AVC capability sets.
 */
export function h264Decoder(factory: (surfaceId: number) => H264Decoder): Extension {
    return new Extension('h264_decoder', factory);
}

// --- Audio output (MS-RDPEA) ---

export interface AudioPlaybackCallbacks {
    /**
     * Plays 16-bit little-endian PCM, interleaved. Returns how many milliseconds of audio are
     * queued, which the server uses to keep audio and video in step.
     */
    wave(data: Uint8Array, sampleRate: number, channels: number): number;
    /** Volume of each channel, 0 to 65535. */
    volume?(left: number, right: number): void;
    /** The server stopped the audio stream. */
    close?(): void;
}

/** Redirects the remote computer's audio to `callbacks`. */
export function audioPlayback(callbacks: AudioPlaybackCallbacks): Extension {
    return new Extension('audio_playback', callbacks);
}

// --- RDCleanPath ---

export { RDCleanPathErrorCode } from './RDCleanPathErrorCode';

// --- File transfer (RDP-specific) ---

export { RdpFileTransferProvider } from './RdpFileTransferProvider';
export type {
    RdpFileTransferProviderOptions,
    TransferProgress,
    FileTransferError,
    DownloadHandle,
    UploadHandle,
    DroppedFile,
} from './RdpFileTransferProvider';
export type { FileInfo, FileContentsRequest, FileContentsResponse } from './FileTransfer';
export { FileContentsFlags } from './FileContentsFlags';

// --- Storage backends ---
// Re-export for consumers who want to configure the storageBackend
// option on RdpFileTransferProviderOptions, implement a custom backend,
// or construct a specific backend instance directly.
export type { FileStorageBackend, FileWriteHandle, StorageBackendPreference } from './storage';
export { BlobStorageBackend } from './storage';
export { OpfsStorageBackend } from './storage';
export { detectStorageBackend } from './storage';

// Re-export extension factories for advanced consumers who want to
// register callbacks or invoke file transfer operations directly.
export {
    filesAvailableCallback,
    fileContentsRequestCallback,
    fileContentsResponseCallback,
    lockCallback,
    unlockCallback,
    locksExpiredCallback,
    requestFileContents,
    submitFileContents,
    initiateFileCopy,
    printJobStreamCallbacks,
    PrinterDriverName,
    printerName,
    printerDeviceId,
    printerDriverName,
} from './extensions';
export type { PrintJobStreamCallbacks } from './extensions';
