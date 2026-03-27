import React, { FC, SVGProps } from 'react';

export const MaximizeIcon: FC<SVGProps<SVGSVGElement>> = ({ width = 20, height = 20 }) => {
    return (
        <svg width={width} height={height} fill="none" viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg">
            <g stroke="currentColor" strokeLinecap="round" strokeLinejoin="round" strokeWidth="2">
                <path d="m15 3h6v6" />
                <path d="m9 21h-6v-6" />
                <path d="m21 3-7 7" />
                <path d="m3 21 7-7" />
            </g>
        </svg>
    );
};
