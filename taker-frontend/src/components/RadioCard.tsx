import { Box, useColorModeValue, useRadio } from "@chakra-ui/react";
import React from "react";

export function RadioCard(props: any) {
    const { getInputProps, getCheckboxProps } = useRadio(props);

    const input = getInputProps();
    const checkbox = getCheckboxProps();

    // TODO: Consider moving these colors into the theme
    const mainColorLight = "gray.600";
    const mainColorDark = "gray.100";

    return (
        <Box as="label">
            <input {...input} />
            <Box
                {...checkbox}
                cursor="pointer"
                borderWidth="1px"
                borderRadius="md"
                boxShadow="md"
                borderColor={useColorModeValue(mainColorLight, mainColorDark)}
                _checked={{
                    bg: useColorModeValue(mainColorLight, mainColorDark),
                    color: useColorModeValue("white", "black"),
                    borderColor: useColorModeValue(mainColorLight, mainColorDark),
                }}
                _focus={{
                    boxShadow: "outline",
                }}
                padding={2}
            >
                {props.children}
            </Box>
        </Box>
    );
}
