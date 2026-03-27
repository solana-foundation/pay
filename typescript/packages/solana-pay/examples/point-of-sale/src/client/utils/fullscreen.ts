declare global {
    interface Document {
        readonly webkitCurrentFullScreenElement?: Element | null;
        readonly webkitFullscreenElement?: Element | null;
        readonly mozFullScreenElement?: Element | null;
        readonly msFullscreenElement?: Element | null;

        readonly fullscreen: boolean;
        readonly webkitIsFullScreen?: boolean;
        readonly mozFullScreen?: boolean;

        webkitExitFullscreen?: () => Promise<void>;
        mozCancelFullScreen?: () => Promise<void>;
        msExitFullscreen?: () => Promise<void>;
    }

    interface HTMLElement {
        webkitRequestFullscreen(options?: FullscreenOptions): Promise<void>;
        mozRequestFullScreen(options?: FullscreenOptions): Promise<void>;
        msRequestFullscreen(options?: FullscreenOptions): Promise<void>;
    }
}

export function isFullscreen(): boolean {
    return (
        typeof document !== 'undefined' &&
        !!(
            document.fullscreenElement ||
            document.webkitCurrentFullScreenElement ||
            document.webkitFullscreenElement ||
            document.mozFullScreenElement ||
            document.msFullscreenElement ||
            document.fullscreen ||
            document.webkitIsFullScreen ||
            document.mozFullScreen
        )
    );
}

export function exitFullscreen(): void {
    const exitFullscreen =
        document.exitFullscreen ||
        document.webkitExitFullscreen ||
        document.mozCancelFullScreen ||
        document.msExitFullscreen;
    if (exitFullscreen) {
        exitFullscreen.call(document).catch((error) => console.warn(error));
    }
}

export function requestFullscreen(): void {
    const requestFullscreen =
        document.documentElement.requestFullscreen ||
        document.documentElement.webkitRequestFullscreen ||
        document.documentElement.mozRequestFullScreen ||
        document.documentElement.msRequestFullscreen;
    if (requestFullscreen) {
        requestFullscreen.call(document.documentElement).catch((error) => console.warn(error));
    }
}

export function toggleFullscreen(): void {
    if (isFullscreen()) {
        exitFullscreen();
    } else {
        requestFullscreen();
    }
}
