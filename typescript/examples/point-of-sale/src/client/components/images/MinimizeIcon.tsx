import React, { FC, SVGProps } from 'react';

export const MinimizeIcon: FC<SVGProps<SVGSVGElement>> = ({ width = 20, height = 20 }) => {
    return (
        <svg width={width} height={height} fill="none" viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg">
            <g stroke="currentColor" strokeLinecap="round" strokeLinejoin="round" strokeWidth="2">
                <path d="m4 14h6v6" />
                <path d="m20 10h-6v-6" />
                <path d="m14 10 7-7" />
                <path d="m3 21 7-7" />
            </g>
        </svg>
    );
};
