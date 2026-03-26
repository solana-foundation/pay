import React from 'react';

export default function Card({ children, link }) {
    return (
        <div className="col col--6 margin-bottom--lg">
            <a className="card padding--lg cardContainer" href={link} target="_blank">
                {children}
            </a>
        </div>
    );
}
