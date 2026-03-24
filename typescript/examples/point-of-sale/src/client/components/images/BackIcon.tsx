import React, { FC, SVGProps } from 'react';

export const BackIcon: FC<SVGProps<SVGSVGElement>> = ({ width = 20, height = 20 }) => {
    return (
        <svg width={width} height={height} fill="none" viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg">
            <path
                d="m15 18-6-6 6-6"
                stroke="currentColor"
                strokeLinecap="round"
                strokeLinejoin="round"
                strokeWidth="2"
            />
        </svg>
    );
};
