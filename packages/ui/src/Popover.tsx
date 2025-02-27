import * as Radix from '@radix-ui/react-popover';
import clsx from 'clsx';
import React, { useEffect, useRef, useState } from 'react';

import { tw } from './utils';

interface Props extends Radix.PopoverContentProps {
	trigger: React.ReactNode;
	disabled?: boolean;
}

export const PopoverContainer = tw.div`flex flex-col p-1.5`;
export const PopoverSection = tw.div`flex flex-col`;
export const PopoverDivider = tw.div`my-2 border-b border-app-line`;

export const Popover = ({ trigger, children, disabled, className, ...props }: Props) => {
	const triggerRef = useRef<HTMLButtonElement>(null);

	const [open, setOpen] = useState(false);

	useEffect(() => {
		const onResize = () => {
			if (triggerRef.current && triggerRef.current.offsetWidth === 0) setOpen(false);
		};

		window.addEventListener('resize', onResize);
		return () => {
			window.removeEventListener('resize', onResize);
		};
	}, []);

	return (
		<Radix.Root open={open} onOpenChange={setOpen}>
			<Radix.Trigger ref={triggerRef} disabled={disabled} asChild>
				{trigger}
			</Radix.Trigger>

			<Radix.Portal>
				<Radix.Content
					onOpenAutoFocus={(event) => event.preventDefault()}
					onCloseAutoFocus={(event) => event.preventDefault()}
					className={clsx(
						'flex flex-col',
						'z-50 m-2 min-w-[11rem]',
						'cursor-default select-none rounded-lg',
						'text-left text-sm text-ink',
						'bg-app-overlay',
						'border border-app-line',
						'shadow-2xl',
						'animate-in fade-in',
						className
					)}
					{...props}
				>
					{children}
				</Radix.Content>
			</Radix.Portal>
		</Radix.Root>
	);
};

export { Close as PopoverClose } from '@radix-ui/react-popover';
