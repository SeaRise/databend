// Copyright 2022 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::io::Write;

use common_expression::types::*;
use common_expression::FromData;
use goldenfile::Mint;

use super::run_ast;

#[test]
fn test_geo_h3() {
    let mut mint = Mint::new("tests/it/scalars/testdata");
    let file = &mut mint.new_goldenfile("geo_h3.txt").unwrap();

    test_h3_to_geo(file);
    test_h3_to_geo_boundary(file);
    test_h3_k_ring(file);
    test_h3_is_valid(file);
    test_h3_get_resolution(file);
    test_h3_edge_length_m(file);
    test_h3_edge_length_km(file);
}

fn test_h3_to_geo(file: &mut impl Write) {
    run_ast(file, "h3_to_geo(-1)", &[]);
    run_ast(file, "h3_to_geo(0)", &[]);
    run_ast(file, "h3_to_geo(1)", &[]);

    run_ast(file, "h3_to_geo(644325524701193974)", &[]);

    run_ast(file, "h3_to_geo(h3)", &[(
        "h3",
        UInt64Type::from_data(vec![
            644325529094369568,
            644325528627451570,
            644325528491955313,
        ]),
    )]);
}

fn test_h3_to_geo_boundary(file: &mut impl Write) {
    run_ast(file, "h3_to_geo_boundary(-1)", &[]);
    run_ast(file, "h3_to_geo_boundary(0)", &[]);
    run_ast(file, "h3_to_geo_boundary(1)", &[]);

    run_ast(file, "h3_to_geo_boundary(644325524701193974)", &[]);

    run_ast(file, "h3_to_geo_boundary(h3)", &[(
        "h3",
        UInt64Type::from_data(vec![
            644325524701193974,
            644325529094369568,
            644325528627451570,
            644325528491955313,
        ]),
    )]);
}

fn test_h3_k_ring(file: &mut impl Write) {
    run_ast(file, "h3_k_ring(-1, 1)", &[]);
    run_ast(file, "h3_k_ring(0, 0)", &[]);
    run_ast(file, "h3_k_ring(0, -1)", &[]);

    run_ast(file, "h3_k_ring(644325524701193974, -1)", &[]);
    run_ast(file, "h3_k_ring(644325524701193974, 0)", &[]);

    run_ast(file, "h3_k_ring(644325524701193974, 1)", &[]);
    run_ast(file, "h3_k_ring(644325524701193974, 2)", &[]);
    run_ast(file, "h3_k_ring(644325524701193974, 3)", &[]);

    run_ast(file, "h3_k_ring(h3, k)", &[
        (
            "h3",
            UInt64Type::from_data(vec![
                644325524701193974,
                644325529094369568,
                644325528627451570,
                644325528491955313,
            ]),
        ),
        ("k", UInt32Type::from_data(vec![1, 2, 3, 4])),
    ]);
}

fn test_h3_is_valid(file: &mut impl Write) {
    run_ast(file, "h3_is_valid(0)", &[]);
    run_ast(file, "h3_is_valid(644325524701193974)", &[]);

    run_ast(file, "h3_is_valid(res)", &[(
        "res",
        UInt64Type::from_data(vec![
            1,
            644325524701193974,
            644325529094369568,
            644325528627451570,
            644325528491955313,
        ]),
    )]);
}

fn test_h3_get_resolution(file: &mut impl Write) {
    run_ast(file, "h3_get_resolution(0)", &[]);
    run_ast(file, "h3_get_resolution(644325524701193974)", &[]);

    run_ast(file, "h3_get_resolution(h3)", &[(
        "h3",
        UInt64Type::from_data(vec![
            644325524701193974,
            644325529094369568,
            644325528627451570,
            644325528491955313,
        ]),
    )]);
}

fn test_h3_edge_length_m(file: &mut impl Write) {
    run_ast(file, "h3_edge_length_m(0)", &[]);
    run_ast(file, "h3_edge_length_m(1)", &[]);
    run_ast(file, "h3_edge_length_m(15)", &[]);
    run_ast(file, "h3_edge_length_m(16)", &[]);

    run_ast(file, "h3_edge_length_m(res)", &[(
        "res",
        UInt8Type::from_data(vec![1, 2, 3, 4]),
    )]);
}

fn test_h3_edge_length_km(file: &mut impl Write) {
    run_ast(file, "h3_edge_length_km(0)", &[]);
    run_ast(file, "h3_edge_length_km(1)", &[]);
    run_ast(file, "h3_edge_length_km(15)", &[]);
    run_ast(file, "h3_edge_length_km(16)", &[]);

    run_ast(file, "h3_edge_length_km(res)", &[(
        "res",
        UInt8Type::from_data(vec![1, 2, 3, 4]),
    )]);
}
