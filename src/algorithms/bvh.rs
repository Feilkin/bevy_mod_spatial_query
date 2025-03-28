//! Bounding Volume Hierarchy -accelerated spatial lookup

use crate::SpatialLookupAlgorithm;
use bevy::math::{FloatOrd, FloatPow};
use bevy::prelude::*;
use bevy::tasks::TaskPool;

type EntityPositionPair = (Entity, Vec3);

/// Bounding Volume Hierarchy -based spatial acceleration algorithm.
///
/// This implementation uses Surface Area Heuristic for splitting the space. Maximum number of
/// splits to sample can be configured with the `max_split_samples_per_axis` field.
///
/// Number of entities per leaf node is controlled by the `entities_per_leaf` field. Storing higher
/// number of entities per field results in smaller tree structure, faster tree building and
/// traversal, but slower final entity filtering.
///
/// Spatial lookups with the BVH structure can be split into two phases: tree traversal and final
/// filtering.
///
/// During the traversal, the BVH tree is traversed starting from the node, and each node
/// that intersects with the query (radius, aabb, etc) is entered.
///
/// For each entered node, if it is a leaf node, each contained entity is then filtered against the
/// query (radius, aabb, etc) to remove entities which are contained in the leaf node but do not
/// actually intersect the query.
#[derive(Debug)]
pub struct Bvh {
    /// Maximum number of entities per leaf node.
    pub entities_per_leaf: usize,
    /// Maximum number of test splits performed per axis. Larger number results in better (=faster)
    /// tree structure but makes tree generation slower.
    pub max_split_samples_per_axis: usize,
    root: Option<BvhNode>,
    tree_depth: usize,
    task_pool: TaskPool,
}

impl Default for Bvh {
    fn default() -> Self {
        Bvh {
            entities_per_leaf: 10_000,
            max_split_samples_per_axis: 10,
            root: None,
            tree_depth: 0,
            task_pool: TaskPool::new(),
        }
    }
}

impl SpatialLookupAlgorithm for Bvh {
    fn prepare(&mut self, entities: &[EntityPositionPair]) {
        let root = split_node(
            entities,
            self.entities_per_leaf,
            self.max_split_samples_per_axis,
            &self.task_pool,
        );

        self.tree_depth = root.count_depth();
        self.root = Some(root);
    }

    fn entities_in_radius(&self, sample_point: Vec3, radius: f32) -> Vec<Entity> {
        if let Some(root) = &self.root {
            root.entities_in_radius(sample_point, radius)
        } else {
            warn!(
                "called Bvh::entities_in_radius before initializing the lookup with Bvh::prepare,\
                no entities will be returned"
            );
            Vec::new()
        }
    }

    fn debug_gizmos(&self, gizmos: &mut Gizmos) {
        if let Some(root) = &self.root {
            root.draw_gizmos(gizmos, 0, self.tree_depth);
        }
    }
}

/// Recursively splits a slice of Entity, Position pairs into BVH nodes.
///
/// This implementation uses the Surface Area Heuristic with a user-controllable amount of
/// split samples.
fn split_node(
    entities: &[EntityPositionPair],
    entities_per_leaf: usize,
    max_split_samples_per_axis: usize,
    task_pool: &TaskPool,
) -> BvhNode {
    assert!(!entities.is_empty());

    // we make a copy of the slice, because we need to sort it to find the axis of best split
    let mut entities = entities.to_vec();
    let aabb = calculate_aabb(&entities);

    if entities.len() <= entities_per_leaf {
        return BvhNode {
            aabb,
            kind: BvhNodeKind::Leaf(entities),
        };
    }

    let sort_by_axis = |axis: usize, entities: &mut [EntityPositionPair]| {
        entities.sort_unstable_by_key(|(_entity, position)| FloatOrd(position[axis]));
    };

    // find the axis of best split
    // TODO: to support 2D BVHs, all we have to do is use the first 2 axis instead of all 3.
    let costs: Vec<(usize, f32)> = (0..3)
        .map(|axis| {
            sort_by_axis(axis, &mut entities);
            find_split_index_and_cost(&entities, max_split_samples_per_axis)
        })
        .collect();

    let (axis, (split_at, _cost)) = costs
        .iter()
        .enumerate()
        .min_by_key(|(_axis, (_split_at, cost))| FloatOrd(*cost))
        .unwrap();

    // split entities at the index of best split
    sort_by_axis(axis, &mut entities);
    let (left, right) = entities.split_at(*split_at);

    let mut nodes = task_pool.scope(|scope| {
        scope.spawn(async move {
            split_node(
                left,
                entities_per_leaf,
                max_split_samples_per_axis,
                task_pool,
            )
        });
        scope.spawn(async move {
            split_node(
                right,
                entities_per_leaf,
                max_split_samples_per_axis,
                task_pool,
            )
        });
    });
    assert_eq!(nodes.len(), 2);
    // Unwrap is fine because of the assert above
    let right_node = nodes.pop().unwrap();
    let left_node = nodes.pop().unwrap();

    BvhNode {
        aabb,
        kind: BvhNodeKind::Branch(Box::new(left_node), Box::new(right_node)),
    }
}

/// Find the best split index and the resulting cost of the sorted `entities` slice.
fn find_split_index_and_cost(
    entities: &[EntityPositionPair],
    max_split_samples_per_axis: usize,
) -> (usize, f32) {
    assert!(entities.len() > 1);

    let samples = entities.len().min(max_split_samples_per_axis);
    let step = entities.len() / samples;

    let mut min = (1, f32::INFINITY);
    for i in (1..entities.len() - 1).step_by(step) {
        let current_cost = cost(entities, i);
        if current_cost < min.1 {
            min = (i, current_cost);
        }
    }

    min
}

/// Surface Area Heuristic.
///
/// The cost is based on the surface areas of the two resulting AABB shapes.
fn cost(entities: &[EntityPositionPair], index: usize) -> f32 {
    let (left, right) = entities.split_at(index);

    let left_aabb = calculate_aabb(left);
    let right_aabb = calculate_aabb(right);

    let left_surface_area = left_aabb.total_surface_area();
    let right_surface_area = right_aabb.total_surface_area();

    let left_cost = left_surface_area * (left.len() as f32);
    let right_cost = right_surface_area * (right.len() as f32);

    left_cost + right_cost
}

/// Calculates the Axis-Aligned Bounding Box for a set of points.
fn calculate_aabb(entities: &[EntityPositionPair]) -> Aabb {
    assert!(!entities.is_empty());

    let mut min_point = entities[0].1;
    let mut max_point = entities[0].1;

    for (_, position) in entities {
        min_point = min_point.min(*position);
        max_point = max_point.max(*position);
    }

    Aabb {
        min: min_point,
        max: max_point,
    }
}

/// Axis-Aligned Bounding Box.
#[derive(Debug, Clone)]
struct Aabb {
    /// Left-bottom corner of the AABB
    min: Vec3,
    /// Top-right corner of the AABB
    max: Vec3,
}

impl Aabb {
    pub fn total_surface_area(&self) -> f32 {
        let extents = self.max - self.min;

        extents.x * extents.y * 2. + extents.x * extents.z * 2. + extents.y * extents.z * 2.
    }
}

#[derive(Debug, Clone)]
enum BvhNodeKind {
    Leaf(Vec<EntityPositionPair>),
    Branch(Box<BvhNode>, Box<BvhNode>),
}

/// Node of the BVH tree.
///
/// Each node contains an AABB (the chosen bounding volume),
/// and either a list of entities or 2 child nodes.
#[derive(Debug, Clone)]
struct BvhNode {
    aabb: Aabb,
    kind: BvhNodeKind,
}

impl BvhNode {
    /// Returns a list of entities that are in radius of the given sample point.
    fn entities_in_radius(&self, sample_point: Vec3, radius: f32) -> Vec<Entity> {
        if !self.intersects_sphere(sample_point, radius) {
            return Vec::new();
        }

        match &self.kind {
            BvhNodeKind::Leaf(entity_position_pairs) => entity_position_pairs
                .iter()
                .filter_map(|(entity, position)| {
                    if position.distance(sample_point) <= radius {
                        Some(*entity)
                    } else {
                        None
                    }
                })
                .collect(),
            BvhNodeKind::Branch(left, right) => {
                let mut total = left.entities_in_radius(sample_point, radius);

                total.extend(right.entities_in_radius(sample_point, radius));

                total
            }
        }
    }

    /// Returns true if this node intersects given sphere.
    #[inline]
    fn intersects_sphere(&self, sample_point: Vec3, radius: f32) -> bool {
        // implementation is based on Jim Arvo's algorithm from "Graphics Gems".
        // http://web.archive.org/web/20100323053111/http://www.ics.uci.edu/~arvo/code/BoxSphereIntersect.c
        let mut dmin = 0.;

        for axis in 0..3 {
            if sample_point[axis] < self.aabb.min[axis] {
                dmin += (sample_point[axis] - self.aabb.min[axis]).squared();
            } else if sample_point[axis] > self.aabb.max[axis] {
                dmin += (sample_point[axis] - self.aabb.max[axis]).squared();
            }
        }

        dmin <= radius.squared()
    }

    fn count_depth(&self) -> usize {
        match &self.kind {
            BvhNodeKind::Leaf(_) => 1,
            BvhNodeKind::Branch(left, right) => 1 + left.count_depth().max(right.count_depth()),
        }
    }

    fn draw_gizmos(&self, gizmos: &mut Gizmos, level: usize, max_depth: usize) {
        let cuboid_centroid = self.aabb.min.midpoint(self.aabb.max);
        let cuboid_scale = Vec3::new(
            self.aabb.max.x - self.aabb.min.x,
            self.aabb.max.y - self.aabb.min.y,
            self.aabb.max.z - self.aabb.min.z,
        );

        match &self.kind {
            BvhNodeKind::Leaf(_) => {
                gizmos.cuboid(
                    Transform::from_translation(cuboid_centroid).with_scale(cuboid_scale),
                    Color::hsv((level as f32) / (max_depth as f32) * 360., 0.8, 1.0),
                );
            }
            BvhNodeKind::Branch(left, right) => {
                left.draw_gizmos(gizmos, level + 1, max_depth);
                right.draw_gizmos(gizmos, level + 1, max_depth);
            }
        }
    }
}
