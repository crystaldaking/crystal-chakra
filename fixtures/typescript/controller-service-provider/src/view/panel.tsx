interface PanelProps {
    title: string;
}

export function Panel(props: PanelProps) {
    return (
        <section>
            <h1>{props.title}</h1>
        </section>
    );
}
