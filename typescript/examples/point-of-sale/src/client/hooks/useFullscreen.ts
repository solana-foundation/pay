import { createContext, useContext } from 'react';

export interface FullscreenContextState {
    fullscreen: boolean;
    toggleFullscreen(): void;
}

export const FullscreenContext = createContext<FullscreenContextState>({} as FullscreenContextState);

export function useFullscreen(): FullscreenContextState {
    return useContext(FullscreenContext);
}
